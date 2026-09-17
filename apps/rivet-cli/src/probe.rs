//! Research probes.
//!
//! Each of these reports a correlation or a tally that a format decision was
//! or could be made from - never a decoded object. They are the instruments
//! `docs/reverse-engineering.md` records measurements from, and they are kept
//! in the CLI rather than a library because their output is a report for a
//! person to read.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    io::{self},
    path::Path,
};

use ifc_export::ExportSettings;
use rvt_container::{
    DEFAULT_DECODE_LIMIT, MarkerEnvelope, MarkerEnvelopeOptions, PartitionReadOptions, RvtContainer,
};
use rvt_model::{
    ELEMENT_TAIL_BYTES, ElementFields, GElementBounds, GElementGraphFields, MemberWalk,
    RECORD_LENGTH_TRAILER_BYTES, RecordHeader, RecordLayout,
};
use rvt_schema::Schema;
// The whole semantic reconstruction moved into `rvt-import`. Glob-imported
// because the probe commands below read the same intermediate the pipeline
// builds, and naming each item here would be a second list to keep in step.
#[allow(clippy::wildcard_imports)] // The binary's own constants.
use crate::*;
#[allow(clippy::wildcard_imports)]
// Sibling modules of one binary; naming each item would be a second list to keep in step.
use crate::{export::*, inspect::*, source::*};
use rvt_import::{
    BODY_BOUNDS_TOLERANCE_FEET, ClassGeometry, ELEMENT_CLASS_FORMAT_TAG, ExportedElement,
    FACE_INSIDE_THE_BOX_FLAG, GFaceMarks, NAME_OFFSET_AGREEMENT, NameCalibration, UNRESOLVED_CLASS,
    body_bounds_residual_feet, body_extent_feet, body_is_placed_in, brep_body_classes,
    brep_class_indexes, calibrate_names, decode_schema_stream, elem_table_ids, faces_extent_feet,
    for_each_member, geometry_statistics, partition_paths, read_name, read_schema,
    recover_elements, schema_class_index, schema_class_is_a, tally_class_geometry,
};

/// Walk one flat `Global/*` stream as a top-level object of a named class,
/// and report where each declared property was read from.
///
/// A `Global/*` payload is not a `Partitions` member record: it has no record
/// header, so nothing narrows its first variable-width field, which is what
/// [`rvt_model::walk_top_level_object`] exists for. Some of these streams open
/// with a two-byte class-index tag before the object's own fields
/// (`Global/PartitionTable` does, `Global/History` does not), so `--skip`
/// takes it off.
///
/// `--output` writes the decoded payload, which is what lets a value be read
/// back at an offset this prints.
pub(crate) fn global_object(
    path: &Path,
    stream: &str,
    class_name: Option<&str>,
    skip: usize,
    rows: usize,
    hex: usize,
    output: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, schema, _) = decode_schema_stream(&raw)?;
    let class = class_name
        .map(|class_name| {
            schema.class_by_name(class_name).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "schema class not found: {}",
                        escape_terminal_text(class_name)
                    ),
                )
            })
        })
        .transpose()?;

    let stored = container.read_stream_with_limit(stream, DEFAULT_DECODE_LIMIT as u64)?;
    let prepared = if stored.len() >= rvt_container::REVIT_STORED_PAGE_BYTES {
        rvt_container::strip_revit_page_checksums(&stored)
    } else {
        stored.clone()
    };
    let decoded = rvt_container::decode_known_framing(&prepared, DEFAULT_DECODE_LIMIT)?;
    let payload = decoded.payload.get(skip..).unwrap_or_default();

    println!("Stream: {} ({} stored bytes)", stream, stored.len());
    println!("Framing: {:?}", decoded.framing);
    println!("Decoded: {} bytes", decoded.payload.len());

    if let Some(class) = class {
        println!(
            "Walking {} of them as {} [{}]",
            payload.len(),
            class.name,
            class.index
        );
        let (walk, trace) = rvt_model::walk_top_level_object_traced(&schema, class.index, payload);
        println!(
            "Consumed: {}, left over: {}, stop: {:?}",
            walk.consumed, walk.remaining, walk.stop
        );
        println!("Properties read (offset +width  class.property):");
        for entry in trace.iter().take(rows) {
            println!(
                "  {:8} +{:<5} {}.{}",
                entry.offset, entry.consumed, entry.class, entry.property
            );
        }
        if trace.len() > rows {
            println!("  ... {} more", trace.len() - rows);
        }
    }
    if hex > 0 {
        let mut head = String::with_capacity(hex * 2);
        for byte in payload.iter().take(hex) {
            let _ = write!(head, "{byte:02x}");
        }
        println!("Head: {head}");
    }
    if let Some(output) = output {
        std::fs::write(output, &decoded.payload)?;
        println!("Wrote the decoded payload to {}", output.display());
    }
    Ok(())
}

pub(crate) fn partition_id_probe(path: &Path) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let element_ids = elem_table_ids(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Global/ElemTable is required for this probe",
        )
    })?;
    let partition_paths = partition_paths(&container);

    let mut total_members = 0_usize;
    let mut prefix_members = 0_usize;
    let mut member_id_hits = 0_usize;
    let mut prefix_values = BTreeSet::new();
    let mut overlapping_ids = BTreeSet::new();

    println!("Partition first-word / element-ID probe:");
    println!("Candidate element IDs: {}", element_ids.len());
    for partition_path in &partition_paths {
        let report =
            container.inspect_partition(partition_path, PartitionReadOptions::default())?;
        let mut partition_prefixes = BTreeSet::new();
        let mut partition_hits = BTreeSet::new();
        let mut partition_member_hits = 0_usize;
        for member in &report.members {
            let Some(prefix) = member.decoded_prefix().get(..4) else {
                continue;
            };
            let value = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
            prefix_members += 1;
            partition_prefixes.insert(value);
            prefix_values.insert(value);
            if element_ids.contains(&value) {
                partition_member_hits += 1;
                member_id_hits += 1;
                partition_hits.insert(value);
                overlapping_ids.insert(value);
            }
        }
        total_members += report.members.len();
        println!(
            "{}	members={}	prefixes={}	member_hits={}	distinct_words={}	distinct_id_hits={}",
            escape_terminal_text(partition_path),
            report.members.len(),
            report
                .members
                .iter()
                .filter(|member| member.decoded_prefix().len() >= 4)
                .count(),
            partition_member_hits,
            partition_prefixes.len(),
            partition_hits.len()
        );
    }

    println!();
    println!("Probe summary:");
    println!("Partitions: {}", partition_paths.len());
    println!("Validated members: {total_members}");
    println!("Members with a first u32: {prefix_members}");
    println!("Members whose first u32 is a candidate ID: {member_id_hits}");
    println!("Distinct first-u32 values: {}", prefix_values.len());
    println!("Distinct overlapping IDs: {}", overlapping_ids.len());
    println!(
        "Element-ID coverage: {}",
        percentage(overlapping_ids.len(), element_ids.len())
    );
    println!(
        "Distinct-word precision: {}",
        percentage(overlapping_ids.len(), prefix_values.len())
    );
    println!(
        "Member hit rate: {}",
        percentage(member_id_hits, prefix_members)
    );
    Ok(())
}

pub(crate) fn schema_prefix_probe(path: &Path, class_name: &str) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, schema, _) = decode_schema_stream(&raw)?;
    let class = schema.class_by_name(class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "schema class not found: {}",
                escape_terminal_text(class_name)
            ),
        )
    })?;
    let class_index = class.index;
    let options = PartitionReadOptions {
        class_prefix_range: Some((class_index, class_index)),
        ..PartitionReadOptions::default()
    };
    let partition_paths = partition_paths(&container);

    let mut total_members = 0_usize;
    let mut members_with_candidates = 0_usize;
    let mut total_candidates = 0_u64;
    for partition_path in &partition_paths {
        let report = container.inspect_partition(partition_path, options)?;
        total_members += report.members.len();
        members_with_candidates += report
            .members
            .iter()
            .filter(|member| member.class_prefix_candidates > 0)
            .count();
        total_candidates = total_candidates.saturating_add(
            report
                .class_prefix_counts
                .get(&class_index)
                .copied()
                .unwrap_or(0),
        );
    }

    println!("Schema-backed partition prefix probe:");
    println!("Class: {}", escape_terminal_text(&class.name));
    println!("Schema index: {class_index}");
    println!("Partitions: {}", partition_paths.len());
    println!("Validated members: {total_members}");
    println!("Members with candidates: {members_with_candidates}");
    println!("Candidate prefixes: {total_candidates}");
    Ok(())
}

/// Aggregate record-boundary evidence collected from marker envelopes.
#[derive(Debug, Default)]
pub(crate) struct EnvelopeStatistics {
    pub(crate) envelopes: usize,
    /// Last `LEADING_PATTERN_BYTES` bytes before a marker.
    pub(crate) leading_patterns: BTreeMap<Vec<u8>, usize>,
    /// Distance between consecutive candidates inside one decoded member.
    pub(crate) candidate_gaps: BTreeMap<u64, usize>,
    /// Little-endian `u32` immediately after a marker.
    pub(crate) following_words: BTreeMap<u32, usize>,
    pub(crate) following_word_samples: usize,
    /// Stride of the ascending `u32` run a marker sits inside, when one exists.
    pub(crate) sequence_strides: BTreeMap<usize, usize>,
}

impl EnvelopeStatistics {
    pub(crate) fn observe(&mut self, envelopes: &[MarkerEnvelope], marker_value: u32) {
        let mut previous: Option<(usize, u64)> = None;
        for envelope in envelopes {
            self.envelopes += 1;
            if let Some(stride) = sequence_stride(envelope, marker_value) {
                *self.sequence_strides.entry(stride).or_default() += 1;
            }

            let leading = envelope.leading();
            if leading.len() >= LEADING_PATTERN_BYTES {
                let pattern = leading[leading.len() - LEADING_PATTERN_BYTES..].to_vec();
                *self.leading_patterns.entry(pattern).or_default() += 1;
            }
            if let Some(word) = envelope.trailing().get(..4) {
                let value = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                *self.following_words.entry(value).or_default() += 1;
                self.following_word_samples += 1;
            }
            // Captured envelopes are a prefix of the candidates in a member,
            // so consecutive captures are consecutive candidates.
            if let Some((member, offset)) = previous {
                if member == envelope.member_index {
                    let gap = envelope.decoded_offset.saturating_sub(offset);
                    *self.candidate_gaps.entry(gap).or_default() += 1;
                }
            }
            previous = Some((envelope.member_index, envelope.decoded_offset));
        }
    }
}

/// Smallest stride at which the marker is one element of a strictly ascending
/// `u32` run. Such a marker is ordinary numeric data, not a record boundary.
pub(crate) fn sequence_stride(envelope: &MarkerEnvelope, marker_value: u32) -> Option<usize> {
    let leading = envelope.leading();
    let trailing = envelope.trailing();
    SEQUENCE_STRIDES.into_iter().find(|stride| {
        let Some(before) = leading
            .len()
            .checked_sub(*stride)
            .and_then(|start| leading.get(start..start + 4))
        else {
            return false;
        };
        let Some(after) = trailing.get(stride - 4..*stride) else {
            return false;
        };
        read_u32(before) < marker_value && read_u32(after) > marker_value
    })
}

pub(crate) fn marker_envelopes(
    path: &Path,
    class_name: &str,
    leading_bytes: usize,
    trailing_bytes: usize,
    max_envelopes: usize,
    dump: usize,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, schema, _) = decode_schema_stream(&raw)?;
    let class = schema.class_by_name(class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "schema class not found: {}",
                escape_terminal_text(class_name)
            ),
        )
    })?;
    let options = PartitionReadOptions {
        marker_envelope: Some(MarkerEnvelopeOptions {
            class_index: class.index,
            leading_bytes,
            trailing_bytes,
            max_envelopes,
        }),
        ..PartitionReadOptions::default()
    };
    let element_ids = elem_table_ids(&container)?;
    let partition_paths = partition_paths(&container);

    println!("Marker envelope capture:");
    println!("Class: {}", escape_terminal_text(&class.name));
    println!("Schema index: {}", class.index);
    println!("Leading/trailing context bytes: {leading_bytes}/{trailing_bytes}");
    println!("Envelope budget per partition: {max_envelopes}");
    match &element_ids {
        Some(ids) => println!("Element-ID reference: Global/ElemTable ({} IDs)", ids.len()),
        None => println!("Element-ID reference: Global/ElemTable not present"),
    }
    println!();

    let mut statistics = EnvelopeStatistics::default();
    let mut total_members = 0_usize;
    let mut members_with_candidates = 0_usize;
    let mut total_candidates = 0_u64;
    let mut budgeted_partitions = 0_usize;
    let mut dumped = Vec::new();

    for partition_path in &partition_paths {
        let report = container.inspect_partition(partition_path, options)?;
        println!(
            "{}\tmembers={}\tmembers_with_candidates={}\tcandidates={}\tenvelopes={}\tbudget_reached={}",
            escape_terminal_text(partition_path),
            report.members.len(),
            report
                .members
                .iter()
                .filter(|member| member.marker_candidates > 0)
                .count(),
            report.marker_candidates,
            report.marker_envelopes.len(),
            report.marker_envelopes_truncated
        );

        statistics.observe(&report.marker_envelopes, u32::from(class.index));
        total_members += report.members.len();
        members_with_candidates += report
            .members
            .iter()
            .filter(|member| member.marker_candidates > 0)
            .count();
        total_candidates = total_candidates.saturating_add(report.marker_candidates);
        budgeted_partitions += usize::from(report.marker_envelopes_truncated);
        for envelope in report.marker_envelopes {
            if dumped.len() >= dump {
                break;
            }
            dumped.push((partition_path.clone(), envelope));
        }
    }

    println!();
    println!("Envelope summary:");
    println!("Partitions: {}", partition_paths.len());
    println!("Validated members: {total_members}");
    println!("Members with candidates: {members_with_candidates}");
    println!("Marker candidates: {total_candidates}");
    println!("Captured envelopes: {}", statistics.envelopes);
    println!("Partitions stopped by envelope budget: {budgeted_partitions}");

    println!();
    println!("Record-boundary evidence:");
    print_leading_patterns(&statistics);
    print_candidate_gaps(&statistics);
    print_following_words(&statistics, element_ids.as_ref());
    print_sequence_strides(&statistics);

    if !dumped.is_empty() {
        println!();
        println!("Envelope dump:");
        for (partition_path, envelope) in &dumped {
            println!(
                "{}\tmember={}\tdecoded_offset={}\tleading={}\tmarker={}\ttrailing={}",
                escape_terminal_text(partition_path),
                envelope.member_index,
                envelope.decoded_offset,
                hex(envelope.leading()),
                hex(envelope.marker()),
                hex(envelope.trailing())
            );
        }
    }
    Ok(())
}

pub(crate) fn print_leading_patterns(statistics: &EnvelopeStatistics) {
    let sampled = statistics.leading_patterns.values().sum::<usize>();
    println!(
        "Envelopes with {LEADING_PATTERN_BYTES} leading bytes: {sampled} (distinct patterns: {})",
        statistics.leading_patterns.len()
    );
    for (pattern, count) in top_counts(&statistics.leading_patterns, HISTOGRAM_ROWS) {
        println!(
            "  leading={}\tcount={count}\tshare={}",
            hex(&pattern),
            percentage(count, sampled)
        );
    }
}

pub(crate) fn print_candidate_gaps(statistics: &EnvelopeStatistics) {
    let sampled = statistics.candidate_gaps.values().sum::<usize>();
    println!(
        "Consecutive same-member candidate gaps: {sampled} (distinct gaps: {})",
        statistics.candidate_gaps.len()
    );
    for (gap, count) in top_counts(&statistics.candidate_gaps, HISTOGRAM_ROWS) {
        println!(
            "  gap={gap}\tcount={count}\tshare={}",
            percentage(count, sampled)
        );
    }
}

pub(crate) fn print_following_words(
    statistics: &EnvelopeStatistics,
    element_ids: Option<&BTreeSet<u32>>,
) {
    println!(
        "Post-marker u32 samples: {} (distinct values: {})",
        statistics.following_word_samples,
        statistics.following_words.len()
    );
    let Some(element_ids) = element_ids else {
        println!("  ElemTable overlap: not evaluated");
        return;
    };
    let matching_values = statistics
        .following_words
        .keys()
        .filter(|value| element_ids.contains(value))
        .count();
    let matching_samples = statistics
        .following_words
        .iter()
        .filter(|(value, _)| element_ids.contains(value))
        .map(|(_, count)| *count)
        .sum::<usize>();
    println!(
        "  Samples whose u32 is a candidate ID: {matching_samples} ({})",
        percentage(matching_samples, statistics.following_word_samples)
    );
    println!(
        "  Distinct values that are candidate IDs: {matching_values} ({})",
        percentage(matching_values, statistics.following_words.len())
    );
}

pub(crate) fn print_sequence_strides(statistics: &EnvelopeStatistics) {
    let sampled = statistics.sequence_strides.values().sum::<usize>();
    println!(
        "Markers inside an ascending u32 run: {sampled} ({})",
        percentage(sampled, statistics.envelopes)
    );
    for (stride, count) in top_counts(&statistics.sequence_strides, HISTOGRAM_ROWS) {
        println!(
            "  run_stride={stride}\tcount={count}\tshare={}",
            percentage(count, sampled)
        );
    }
}

/// Counters for the two member-level structures under test.
#[derive(Debug, Default)]
pub(crate) struct MemberFramingStatistics {
    pub(crate) members: usize,
    pub(crate) descriptors: usize,
    pub(crate) stored_span_matches: usize,
    pub(crate) chain_links: usize,
    pub(crate) chain_links_checked: usize,
    /// Distance between the end of the previous member and this descriptor.
    pub(crate) descriptor_gaps: BTreeMap<u64, usize>,
    pub(crate) walk_failures_at_page_limit: usize,
    pub(crate) known_format_tags: usize,
    pub(crate) format_tags: BTreeMap<u32, usize>,
    pub(crate) walked: usize,
    pub(crate) walk_failures: BTreeMap<String, usize>,
    pub(crate) count_matches: usize,
    pub(crate) body_matches: usize,
    pub(crate) records: u64,
    /// Members whose payload ends inside a record continuing into the next one.
    pub(crate) continued_members: usize,
    /// Members that resumed after a carried tail.
    pub(crate) resumed_members: usize,
    /// Carried tails consumed by a member that then ended on a record boundary.
    pub(crate) continuations_closed: usize,
    /// Carries abandoned because the next member could not be walked.
    pub(crate) continuations_dropped: usize,
}

pub(crate) fn member_framing(
    path: &Path,
    partition: Option<&str>,
    max_member_bytes: u64,
    dump: usize,
    mismatches: usize,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let partition_paths = match partition {
        Some(name) => vec![name.to_owned()],
        None => partition_paths(&container),
    };
    let mut statistics = MemberFramingStatistics::default();
    let mut dumped = 0_usize;
    let mut mismatches_printed = 0_usize;

    println!("Member framing:");
    for partition_path in &partition_paths {
        let report =
            container.inspect_partition(partition_path, PartitionReadOptions::default())?;
        let mut previous: Option<&rvt_container::PartitionMember> = None;
        // Bytes the previous member's last record still expects.
        let mut carry = 0_u64;
        for member in &report.members {
            statistics.members += 1;
            let Some(descriptor) = member.descriptor else {
                previous = Some(member);
                statistics.continuations_dropped += usize::from(carry > 0);
                carry = 0;
                continue;
            };
            statistics.descriptors += 1;
            statistics.stored_span_matches +=
                usize::from(descriptor.matches_stored_span(member.compressed_bytes));
            if let Some(previous) = previous {
                let gap = member
                    .logical_offset
                    .saturating_sub(previous.logical_offset + previous.compressed_bytes);
                *statistics.descriptor_gaps.entry(gap).or_default() += 1;
                statistics.chain_links_checked += 1;
                let decoded_matches =
                    u64::from(descriptor.previous_decoded_bytes) == previous.decoded_bytes;
                let span_matches = previous
                    .descriptor
                    .is_some_and(|earlier| descriptor.previous_stored_span == earlier.stored_span);
                statistics.chain_links += usize::from(decoded_matches && span_matches);
            }
            *statistics
                .format_tags
                .entry(descriptor.format_tag)
                .or_default() += 1;

            if let Some(layout) = RecordLayout::from_format_tag(descriptor.format_tag) {
                statistics.known_format_tags += 1;
                carry = walk_member(
                    &container,
                    partition_path,
                    member,
                    descriptor,
                    layout,
                    max_member_bytes,
                    carry,
                    dump,
                    &mut dumped,
                    (mismatches, &mut mismatches_printed),
                    &mut statistics,
                )?;
            } else {
                statistics.continuations_dropped += usize::from(carry > 0);
                carry = 0;
            }
            previous = Some(member);
        }
    }
    print_member_framing(&statistics, partition_paths.len());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_member(
    container: &RvtContainer,
    partition_path: &str,
    member: &rvt_container::PartitionMember,
    descriptor: rvt_container::MemberDescriptor,
    layout: RecordLayout,
    max_member_bytes: u64,
    carry: u64,
    dump: usize,
    dumped: &mut usize,
    (mismatch_limit, mismatches_printed): (usize, &mut usize),
    statistics: &mut MemberFramingStatistics,
) -> Result<u64, Box<dyn Error>> {
    let payload = container.decode_partition_member(
        partition_path,
        member.logical_offset,
        max_member_bytes,
    )?;
    let leading_carry = usize::try_from(carry).unwrap_or(usize::MAX);
    match MemberWalk::parse(&payload, layout, leading_carry) {
        Ok(walk) => {
            statistics.walked += 1;
            statistics.records = statistics.records.saturating_add(walk.records.len() as u64);
            statistics.count_matches +=
                usize::from(u64::from(descriptor.record_count) == walk.records.len() as u64);
            let matches =
                walk.matches_descriptor(descriptor.record_count, descriptor.record_body_bytes);
            statistics.body_matches += usize::from(matches);
            if !matches && *mismatches_printed < mismatch_limit {
                *mismatches_printed += 1;
                println!(
                    "  mismatch\t{}\tmember={}\tpayload={}\tcarry={}\trecords={}\tdeficit={}\tbody_declared={}\tbody_in_member={}\tbody_started={}",
                    escape_terminal_text(partition_path),
                    member.index,
                    payload.len(),
                    walk.leading_carry,
                    walk.records.len(),
                    walk.trailing_deficit,
                    descriptor.record_body_bytes,
                    walk.body_bytes_in_member,
                    walk.body_bytes
                );
            }
            statistics.continued_members += usize::from(walk.trailing_deficit > 0);
            if carry > 0 {
                statistics.resumed_members += 1;
                statistics.continuations_closed += usize::from(walk.trailing_deficit == 0);
            }
            if *dumped < dump {
                *dumped += 1;
                println!(
                    "  {}\tmember={}\tformat={}\tcarry={}\trecords={}\tdeclared={}\tbody={}\tdeclared_body={}\tdeficit={}",
                    escape_terminal_text(partition_path),
                    member.index,
                    descriptor.format_tag,
                    walk.leading_carry,
                    walk.records.len(),
                    descriptor.record_count,
                    walk.body_bytes_in_member,
                    descriptor.record_body_bytes,
                    walk.trailing_deficit
                );
                for record in walk.records.iter().take(4) {
                    println!(
                        "    record\toffset={}\theader={}\tbody={}",
                        record.offset, record.header_bytes, record.body_bytes
                    );
                }
            }
            Ok(walk.trailing_deficit)
        }
        Err(error) => {
            *statistics
                .walk_failures
                .entry(error.to_string())
                .or_default() += 1;
            statistics.walk_failures_at_page_limit +=
                usize::from(member.decoded_bytes >= MEMBER_PAGE_LIMIT_BYTES);
            statistics.continuations_dropped += usize::from(carry > 0);
            Ok(0)
        }
    }
}

pub(crate) fn print_member_framing(statistics: &MemberFramingStatistics, partitions: usize) {
    println!();
    println!("Descriptor summary:");
    println!("Partitions: {partitions}");
    println!("Validated members: {}", statistics.members);
    println!("Members with a descriptor: {}", statistics.descriptors);
    println!(
        "Stored span == compressed + 16: {} ({})",
        statistics.stored_span_matches,
        percentage(statistics.stored_span_matches, statistics.descriptors)
    );
    println!(
        "Back-pointers matching the previous member: {} ({})",
        statistics.chain_links,
        percentage(statistics.chain_links, statistics.chain_links_checked)
    );
    print!("Descriptor gaps:");
    for (gap, count) in &statistics.descriptor_gaps {
        print!(" {gap}={count}");
    }
    println!();
    print!("Format tags:");
    for (tag, count) in &statistics.format_tags {
        print!(" {tag}={count}");
    }
    println!();

    println!();
    println!("Record-array summary:");
    println!(
        "Members with a known format tag: {}",
        statistics.known_format_tags
    );
    println!(
        "Members walked without an error: {} ({})",
        statistics.walked,
        percentage(statistics.walked, statistics.known_format_tags)
    );
    println!(
        "Members ending inside a record: {}",
        statistics.continued_members
    );
    println!(
        "Members resuming after a carried tail: {} (ending on a record boundary: {})",
        statistics.resumed_members, statistics.continuations_closed
    );
    println!(
        "Carries dropped at a walk failure: {}",
        statistics.continuations_dropped
    );
    println!(
        "Record count == descriptor count: {} ({})",
        statistics.count_matches,
        percentage(statistics.count_matches, statistics.walked)
    );
    println!(
        "Body bytes == descriptor body bytes: {} ({})",
        statistics.body_matches,
        percentage(statistics.body_matches, statistics.walked)
    );
    println!("Records recovered: {}", statistics.records);
    let failures = statistics.known_format_tags - statistics.walked;
    println!(
        "Walk failures on members at the {} KiB limit: {} of {failures}",
        MEMBER_PAGE_LIMIT_BYTES / 1024,
        statistics.walk_failures_at_page_limit
    );
    for (reason, count) in top_counts(&statistics.walk_failures, HISTOGRAM_ROWS) {
        println!(
            "  failure\tcount={count}\treason={}",
            escape_terminal_text(&reason)
        );
    }
}

/// Which identifiers a record names and never writes an object for.
///
/// A node reaches the stream by being *fully* referenced - identifier plus
/// class - because that is what the walk queues; a bare identifier
/// (`GEdge.m_next`, `GEdgeLoop.m_pFace`) names an object without queueing one.
/// So an object nothing full-references is never written, and the boundary of
/// a face whose `m_pFirstLoop` is null is missing for exactly that reason: the
/// only full reference to its loop was that null.
///
/// This asks what those unwritten identifiers are. Per record: which slots
/// name them, whether the written identifiers form a dense range with the
/// unwritten ones as its gaps, and whether the record's neighbours in the same
/// member write the objects it is missing.
#[allow(clippy::too_many_lines)] // One streaming pass and the tallies it fills.
pub(crate) fn identifier_probe(
    path: &Path,
    rows: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let face_class_index = schema_class_index(Some(&schema), "Face");
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement");
    let (Some(face_class_index), Some(geometry_element_class_index)) =
        (face_class_index, geometry_element_class_index)
    else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare Face and GElement",
        )));
    };
    let partition_paths = partition_paths(&container);
    let class_name = |class_index: u16| {
        schema.class_by_index(class_index).map_or_else(
            || format!("class {class_index}"),
            |class| class.name.clone(),
        )
    };

    let mut records = 0_u64;
    let mut written = 0_u64;
    let mut distinct_written = 0_u64;
    let mut named = 0_u64;
    let mut unwritten = 0_u64;
    // Which slot names an identifier no object is written for, and which slot
    // names one that is written - the same tally twice, so a slot that only
    // ever names the missing can be told from one that usually resolves.
    let mut slots = [BTreeMap::<String, u64>::new(), BTreeMap::new()];
    // Identifiers are not a dense counter - the highest one a record writes is
    // far above the number it writes - so "an identifier below the highest" is
    // no evidence of anything, and this records only that.
    let mut highest = 0_u64;
    // Whether another record of the same member writes what this one misses.
    let mut unwritten_in_member = 0_u64;
    let mut written_by_a_neighbour = 0_u64;
    let mut neighbour_classes = BTreeMap::<String, u64>::new();
    // An element writes more than one `GElement` record, and the export keeps
    // one of them. Per element: how many of its records carry a face with no
    // loop, and how many faces each record holds - so a record missing
    // boundaries can be checked against its own siblings.
    let mut element_records: BTreeMap<u32, Vec<(usize, usize)>> = BTreeMap::new();
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            // Every identifier this member writes an object for, and every one
            // its records name and do not write.
            let mut member_written: BTreeMap<u32, u16> = BTreeMap::new();
            let mut member_missing: BTreeSet<u32> = BTreeSet::new();
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (_, objects) =
                    rvt_model::walk_record_collecting(&schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == face_class_index)
                {
                    continue;
                }
                records += 1;
                written += objects.len() as u64;
                let record_written: BTreeMap<u32, u16> = objects
                    .iter()
                    .map(|object| (object.object_id, object.class_index))
                    .collect();
                distinct_written += record_written.len() as u64;
                member_written.extend(&record_written);

                element_records.entry(header.id).or_default().push((
                    objects
                        .iter()
                        .filter(|object| object.class_index == face_class_index)
                        .count(),
                    objects
                        .iter()
                        .filter(|object| {
                            object.class_index == face_class_index
                                && object
                                    .references
                                    .first()
                                    .is_none_or(|reference| reference.object_id == 0)
                        })
                        .count(),
                ));

                let mut record_named: BTreeSet<u32> = BTreeSet::new();
                for object in &objects {
                    for (slot, identifier) in object.identifiers.iter().enumerate() {
                        if *identifier == 0 {
                            continue;
                        }
                        record_named.insert(*identifier);
                        let resolved = record_written.contains_key(identifier);
                        *slots[usize::from(resolved)]
                            .entry(format!("{} slot {slot}", class_name(object.class_index)))
                            .or_default() += 1;
                    }
                }
                named += record_named.len() as u64;
                let missing = record_named
                    .iter()
                    .filter(|identifier| !record_written.contains_key(identifier))
                    .copied()
                    .collect::<Vec<_>>();
                unwritten += missing.len() as u64;
                member_missing.extend(&missing);

                if let Some(top) = record_written.keys().next_back() {
                    highest = highest.max(u64::from(*top));
                }
            }
            for identifier in &member_missing {
                unwritten_in_member += 1;
                if let Some(class_index) = member_written.get(identifier) {
                    written_by_a_neighbour += 1;
                    *neighbour_classes
                        .entry(class_name(*class_index))
                        .or_default() += 1;
                }
            }
        },
    )?;

    println!("Face-bearing `GElement` records: {records}");
    println!("  objects written: {written} ({distinct_written} distinct identifiers)");
    println!("  identifiers named by a bare reference: {named}, of them unwritten: {unwritten}");
    println!("  highest identifier written by any record: {highest}");
    println!(
        "  distinct unwritten identifiers per member: {unwritten_in_member}, \
         written by another record of the same member: {written_by_a_neighbour}"
    );
    let mut neighbours = neighbour_classes.into_iter().collect::<Vec<_>>();
    neighbours.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (class, count) in neighbours.into_iter().take(rows) {
        println!("  {count}\t{}", escape_terminal_text(&class));
    }
    // An element whose every record is short of boundaries has nowhere else to
    // look; one with a whole sibling has the geometry somewhere in the file.
    let mut elements = [0_u64; 4];
    let mut faces_lost = [0_u64; 2];
    for records in element_records.values() {
        let short = records.iter().filter(|(_, missing)| *missing > 0).count();
        let whole = records.len() - short;
        let index = match (short, whole) {
            (0, _) => 0,
            (_, 0) => 1,
            _ => 2,
        };
        elements[index] += 1;
        elements[3] += u64::from(records.len() > 1);
        if index != 0 {
            faces_lost[usize::from(whole > 0)] += records
                .iter()
                .map(|(_, missing)| *missing as u64)
                .sum::<u64>();
        }
    }
    println!(
        "\nElements by what their `GElement` records carry: {} whole, {} short in every record, \
         {} short in some and whole in another ({} elements write more than one record)",
        elements[0], elements[1], elements[2], elements[3]
    );
    println!(
        "  loopless faces in elements with no whole record: {}, and in elements that have one: {}",
        faces_lost[0], faces_lost[1]
    );
    println!("\nSlots naming an identifier no object is written for:");
    print_scalar_rows(&slots[0], rows);
    println!("\nSlots naming an identifier that is written:");
    print_scalar_rows(&slots[1], rows);
    Ok(())
}

/// How a `Face` reaches the `EdgeLoop` that bounds it.
///
/// `assemble_face` reads the loop from the face's own first reference, and
/// where that reference is missing the face is dropped: on AR S1 that is
/// 76 510 of the 80 205 excluded faces, the largest exclusion by an order of
/// magnitude. Every `EdgeLoop` also declares the face it bounds in
/// `identifiers[0]` - `walk_loop` already refuses a loop whose `pFace` is not
/// its face - so the same link exists on the loop's side, and this measures
/// whether reading it backwards is the same link.
///
/// Two questions, and the first has to be answered before the second means
/// anything. On faces that *do* carry a reference the inverse map must
/// reproduce it: that is the control, and a disagreement there refutes the
/// map. Only then does the count of loops claiming a face that carries no
/// reference say what could be recovered. Both are split on whether the
/// record's declarations tiled its body exactly, because a drifted walk hands
/// this arbitrary bytes rather than the fields it names.
#[allow(clippy::too_many_lines)] // One streaming pass and the tallies it fills.
pub(crate) fn loop_owner_probe(
    path: &Path,
    rows: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let classes = brep_class_indexes(Some(&schema));
    let body_classes = brep_body_classes(Some(&schema));
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement");
    let (Some(classes), Some(geometry_element_class_index)) =
        (classes, geometry_element_class_index)
    else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare the boundary-representation classes",
        )));
    };
    let partition_paths = partition_paths(&container);

    // Every count is (all records, exactly-tiled records only).
    let mut faces = [0_u64; 2];
    let mut referenced = [0_u64; 2];
    let mut control = BTreeMap::<&'static str, [u64; 2]>::new();
    let mut unreferenced = BTreeMap::<&'static str, [u64; 2]>::new();
    // What the first reference of a face that carries no usable one actually
    // holds, so the sentinel can be recognised rather than guessed at.
    let mut first_reference = BTreeMap::<String, u64>::new();
    let mut excluded_without_a_loop = [0_u64; 2];
    let mut recoverable = [0_u64; 2];
    // How many further references a face with a null first one carries, and
    // whether such faces come a whole record at a time or mixed in among
    // faces that do reach a loop.
    let mut reference_counts = BTreeMap::<usize, u64>::new();
    let mut record_split = BTreeMap::<&'static str, u64>::new();
    // What a face with no loop still names: its surface, and whether that
    // surface or its own identifier is one a loop-bearing face in the same
    // record already used.
    let mut null_surface_class = BTreeMap::<String, u64>::new();
    let mut shares_a_surface = 0_u64;
    let mut shares_an_identifier = 0_u64;
    // The scalar fields a face declares - `m_cutType`, `m_faceFlags_v9` - for
    // each kind of face. If the two kinds differ in a declared field, the file
    // is saying what the loopless ones are.
    let mut null_scalars = BTreeMap::<String, u64>::new();
    let mut loop_bearing_scalars = BTreeMap::<String, u64>::new();
    let mut loop_bearing_reference_counts = BTreeMap::<usize, u64>::new();
    // Who names each kind of face, by the class of the naming object and the
    // slot the name sits in. A face is reached by the walk because something
    // referenced it, and if the two kinds are reached from different places
    // that is what they are.
    // Whether a `GBRep` node holds a shell or a set of free surfaces. Every
    // `Edge` names the two faces it separates, so the solid's faces are the
    // ones some edge names; a node no edge reaches declares no shell. What
    // such a node would be is already written down in `declared_bodies`: one
    // node per free surface beside the solid, which is how a wall stores the
    // planes of its compound structure. If the loopless faces fall that way -
    // a whole node at a time - they are not a boundary the walk failed to
    // read, they are a different declaration being counted as one.
    // The mark that already separates a wall's own faces from the cut
    // geometry joined into its shell, asked of both kinds of face - see
    // `FACE_INSIDE_THE_BOX_FLAG`. If the loopless faces are the cut side, the
    // file has already said so and this is not a boundary to go looking for.
    // Whether a face that declares no loop is nonetheless ordered by the file.
    // `GEdge` names, for each of the two faces it separates, the next edge
    // around that face - `identifiers[2 + side]`. Where those are live the ring
    // is declared and the endpoint reconstruction is not needed; where they are
    // null there is nothing to read and an ambiguous corner has to be refused.
    // This is the question to settle before anything is invented for the 4 353
    // faces that corner refuses.
    let mut loopless_next_links = BTreeMap::<&'static str, u64>::new();
    let mut loopless_flags = BTreeMap::<String, u64>::new();
    let mut loop_bearing_flags = BTreeMap::<String, u64>::new();
    let mut node_kind = BTreeMap::<&'static str, u64>::new();
    let mut node_faces_by_kind = BTreeMap::<&'static str, u64>::new();
    // Faces per node, for the nodes no edge reaches: a free surface should be
    // one face, and a count that is not says the reading is something else.
    let mut free_node_faces = BTreeMap::<usize, u64>::new();
    let mut free_nodes_per_record = BTreeMap::<usize, u64>::new();
    // The loopless faces, split by whether their node holds a shell. Only the
    // ones in a shell node are a boundary this walk owes an answer for.
    let mut loopless_by_node = BTreeMap::<&'static str, u64>::new();
    let mut null_parents = BTreeMap::<String, u64>::new();
    let mut loop_bearing_parents = BTreeMap::<String, u64>::new();
    // Whether the shell the loop-bearing faces make is self-contained. Every
    // `Edge` names the two faces it separates, so an edge naming a loopless
    // face says that face is part of the solid and its boundary is missing;
    // no such edge says the loopless faces are not in the shell at all.
    let mut edges_naming = BTreeMap::<&'static str, u64>::new();
    // Whether a face's boundary can be rebuilt from the edges alone. Every
    // `Edge` names the two faces it separates and, for each of them, the next
    // edge around that face, so the ring is in the edges and the `EdgeLoop`
    // object is only an entry point into it. Asked of both kinds of face: on
    // the ones that have a loop the rebuild must reproduce it, and on the ones
    // that have none it is the only route there is.
    let mut rings = [BTreeMap::<String, u64>::new(), BTreeMap::new()];
    // Identifiers are record-local, so two objects of one class sharing one
    // identifier inside a record would make every map here cross two objects.
    let mut duplicate_identifiers = BTreeMap::<&'static str, u64>::new();
    // What a ring's last edge steps onto when it is not one of the loops that
    // claim the face: an object of some other class, or nothing this record
    // holds at all.
    let mut terminators = BTreeMap::<String, u64>::new();
    let mut terminator_range = BTreeMap::<&'static str, u64>::new();
    // How many edges each ring has, split on whether an `EdgeLoop` of the
    // record ends it. A ring of one or two edges is a broken chain rather than
    // a hole, so this is the check on the rings no loop knows about.
    let mut ring_lengths = [BTreeMap::<usize, u64>::new(), BTreeMap::new()];
    // The same rebuild in three numbers per kind of face: rings recovered,
    // no edge to recover them from, and a rebuild that failed outright.
    let mut rebuilt_tally = [[0_u64; 3]; 2];
    // Rings recovered beyond the loops the record actually holds - a face
    // whose holes are in the edges and in no `EdgeLoop` object.
    let mut rings_beyond_the_loops = [0_u64; 2];
    // What ordering the edges of a loopless face geometrically could reach.
    // A body is emitted whole or not at all, so the population that matters is
    // records, not faces: a record where one loopless face has no edge to
    // order cannot be completed however well the others go.
    let mut short_records = 0_u64;
    let mut every_loopless_face_has_edges = 0_u64;
    let mut short_only_for_want_of_a_loop = 0_u64;
    let mut recoverable_records = 0_u64;
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (record_walk, objects) =
                    rvt_model::walk_record_collecting(&schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == classes.face)
                {
                    continue;
                }
                let exact = record_walk.is_exact();
                let bump = |counter: &mut [u64; 2]| {
                    counter[0] += 1;
                    counter[1] += u64::from(exact);
                };

                // Which loops claim each face, read from `EdgeLoop.pFace`. A
                // loop that does not declare all three of pFace/next/prev is
                // one `walk_loop` would refuse anyway, so it claims nothing.
                let mut claimed: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                for object in &objects {
                    if object.class_index != classes.edge_loop || object.identifiers.len() != 3 {
                        continue;
                    }
                    claimed
                        .entry(object.identifiers[0])
                        .or_default()
                        .push(object.object_id);
                }

                for face in objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                {
                    bump(&mut faces);
                    let claiming = claimed.get(&face.object_id).map_or(&[][..], Vec::as_slice);
                    if let Some(reference) = face
                        .references
                        .first()
                        .filter(|reference| reference.object_id != 0)
                    {
                        bump(&mut referenced);
                        let verdict = match claiming {
                            [] => "no loop claims the face",
                            [only] if *only == reference.object_id => {
                                "one loop, and it is the referenced one"
                            }
                            [_] => "one loop, and it is not the referenced one",
                            many if many.contains(&reference.object_id) => {
                                "several loops, the referenced one among them"
                            }
                            _ => "several loops, none of them the referenced one",
                        };
                        bump(control.entry(verdict).or_default());
                    } else {
                        let verdict = match claiming {
                            [] => "no loop claims the face",
                            [_] => "exactly one loop claims the face",
                            _ => "several loops claim the face",
                        };
                        bump(unreferenced.entry(verdict).or_default());
                        *reference_counts.entry(face.references.len()).or_default() += 1;
                        *first_reference
                            .entry(face.references.first().map_or_else(
                                || "no references at all".to_owned(),
                                |reference| {
                                    format!(
                                        "first reference (id {}, class {})",
                                        reference.object_id, reference.class_index
                                    )
                                },
                            ))
                            .or_default() += 1;
                    }
                }

                // Every object naming a face of this record, by the class
                // that names it and the slot the name sits in.
                let mut named_by: BTreeMap<u32, Vec<String>> = BTreeMap::new();
                for object in &objects {
                    for (slot, reference) in object.references.iter().enumerate() {
                        if reference.object_id == 0 || reference.class_index != classes.face {
                            continue;
                        }
                        named_by
                            .entry(reference.object_id)
                            .or_default()
                            .push(format!(
                                "{} slot {slot}",
                                schema.class_by_index(object.class_index).map_or_else(
                                    || format!("class {}", object.class_index),
                                    |class| class.name.clone()
                                )
                            ));
                    }
                }

                // Which faces an edge names, and which faces each node holds.
                let mut edge_named: BTreeSet<u32> = BTreeSet::new();
                for edge in objects.iter().filter(|object| {
                    object.class_index == classes.edge && object.identifiers.len() == 6
                }) {
                    for face in &edge.identifiers[..2] {
                        if *face != 0 {
                            edge_named.insert(*face);
                        }
                    }
                }
                let mut node_of_face: BTreeMap<u32, u32> = BTreeMap::new();
                let mut faces_of_node: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                for node in objects
                    .iter()
                    .filter(|object| body_classes.contains(&object.class_index))
                {
                    for reference in &node.references {
                        if reference.class_index != classes.face || reference.object_id == 0 {
                            continue;
                        }
                        node_of_face
                            .entry(reference.object_id)
                            .or_insert(node.object_id);
                        faces_of_node
                            .entry(node.object_id)
                            .or_default()
                            .push(reference.object_id);
                    }
                }
                let mut edges_of_face: BTreeMap<u32, Vec<(u32, usize)>> = BTreeMap::new();
                for edge in objects.iter().filter(|object| {
                    object.class_index == classes.edge && object.identifiers.len() == 6
                }) {
                    for (side, face) in edge.identifiers[..2].iter().enumerate() {
                        if *face != 0 {
                            edges_of_face
                                .entry(*face)
                                .or_default()
                                .push((edge.object_id, side));
                        }
                    }
                }
                let by_edge_id: BTreeMap<u32, &rvt_model::SerialObject> = objects
                    .iter()
                    .filter(|object| object.class_index == classes.edge)
                    .map(|object| (object.object_id, object))
                    .collect();
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_none_or(|reference| reference.object_id == 0)
                }) {
                    let Some(incidences) = edges_of_face.get(&face.object_id) else {
                        continue;
                    };
                    let live = incidences
                        .iter()
                        .filter(|(edge, side)| {
                            by_edge_id.get(edge).is_some_and(|edge| {
                                edge.identifiers
                                    .get(2 + side)
                                    .is_some_and(|next| *next != 0)
                            })
                        })
                        .count();
                    let verdict = if live == incidences.len() {
                        "every edge names the next one around this face"
                    } else if live == 0 {
                        "no edge names a next one around this face"
                    } else {
                        "some do and some do not"
                    };
                    *loopless_next_links.entry(verdict).or_default() += 1;
                }
                let mut free_nodes = 0_usize;
                let mut free_node_ids: BTreeSet<u32> = BTreeSet::new();
                for (node, node_face_ids) in &faces_of_node {
                    let bounded = node_face_ids
                        .iter()
                        .filter(|face| edge_named.contains(face))
                        .count();
                    let kind = if bounded == node_face_ids.len() {
                        "every face of the node is named by an edge"
                    } else if bounded == 0 {
                        "no face of the node is named by an edge"
                    } else {
                        "some faces of the node are named by an edge and some are not"
                    };
                    *node_kind.entry(kind).or_default() += 1;
                    *node_faces_by_kind.entry(kind).or_default() += node_face_ids.len() as u64;
                    if bounded == 0 {
                        free_nodes += 1;
                        free_node_ids.insert(*node);
                        *free_node_faces.entry(node_face_ids.len()).or_default() += 1;
                    }
                }
                *free_nodes_per_record.entry(free_nodes).or_default() += 1;
                for face in objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                {
                    let loopless = face
                        .references
                        .first()
                        .is_none_or(|reference| reference.object_id == 0)
                        && !edge_named.contains(&face.object_id);
                    // Read through `GFaceMarks` rather than off an index, so
                    // this asks the same question `place_body_less_its_cut_faces`
                    // asks and a face whose walk was truncated is not read as
                    // one declaring zeroes.
                    let marks = GFaceMarks::read(face);
                    let key = format!(
                        "{} / {:?}",
                        match marks {
                            Some(marks) if marks.info_flags & FACE_INSIDE_THE_BOX_FLAG != 0 =>
                                "inside the box",
                            Some(_) => "not inside the box",
                            None => "the face's marks did not all read",
                        },
                        face.alternate_integers
                    );
                    *if loopless {
                        loopless_flags.entry(key).or_default()
                    } else {
                        loop_bearing_flags.entry(key).or_default()
                    } += 1;
                }
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_none_or(|reference| reference.object_id == 0)
                        && !edge_named.contains(&object.object_id)
                }) {
                    let verdict = match node_of_face.get(&face.object_id) {
                        None => "no node names the face",
                        Some(node) if free_node_ids.contains(node) => "in a node no edge reaches",
                        Some(_) => "in a node that holds a shell",
                    };
                    *loopless_by_node.entry(verdict).or_default() += 1;
                }

                // Every surface and identifier a loop-bearing face in this
                // record claims, to test the null-loop faces against.
                let mut loop_bearing_surfaces = BTreeSet::new();
                let mut loop_bearing_faces = BTreeSet::new();
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_some_and(|reference| reference.object_id != 0)
                }) {
                    loop_bearing_faces.insert(face.object_id);
                    *loop_bearing_reference_counts
                        .entry(face.references.len())
                        .or_default() += 1;
                    *loop_bearing_scalars.entry(face_scalars(face)).or_default() += 1;
                    *loop_bearing_parents
                        .entry(named_by.get(&face.object_id).map_or_else(
                            || "nothing names it".to_owned(),
                            |names| names.join(", "),
                        ))
                        .or_default() += 1;
                    if let Some(surface) = face.references.last() {
                        loop_bearing_surfaces.insert((surface.object_id, surface.class_index));
                    }
                }
                for face in objects.iter().filter(|object| {
                    object.class_index == classes.face
                        && object
                            .references
                            .first()
                            .is_none_or(|reference| reference.object_id == 0)
                }) {
                    let surface = face.references.last();
                    *null_surface_class
                        .entry(surface.map_or_else(
                            || "no surface reference".to_owned(),
                            |surface| {
                                if surface.object_id == 0 {
                                    "null surface reference".to_owned()
                                } else if surface.class_index == classes.plane {
                                    "Plane".to_owned()
                                } else if surface.class_index == classes.cyl_surf {
                                    "CylSurf".to_owned()
                                } else {
                                    format!("class {}", surface.class_index)
                                }
                            },
                        ))
                        .or_default() += 1;
                    if surface.is_some_and(|surface| {
                        loop_bearing_surfaces.contains(&(surface.object_id, surface.class_index))
                    }) {
                        shares_a_surface += 1;
                    }
                    if loop_bearing_faces.contains(&face.object_id) {
                        shares_an_identifier += 1;
                    }
                    *null_scalars.entry(face_scalars(face)).or_default() += 1;
                    *null_parents
                        .entry(named_by.get(&face.object_id).map_or_else(
                            || "nothing names it".to_owned(),
                            |names| names.join(", "),
                        ))
                        .or_default() += 1;
                }

                {
                    let mut seen = BTreeMap::<(u16, u32), u64>::new();
                    for object in &objects {
                        *seen
                            .entry((object.class_index, object.object_id))
                            .or_default() += 1;
                    }
                    for ((class_index, _), count) in seen {
                        if count > 1 {
                            *duplicate_identifiers
                                .entry(if class_index == classes.face {
                                    "Face"
                                } else if class_index == classes.edge {
                                    "Edge"
                                } else if class_index == classes.edge_loop {
                                    "EdgeLoop"
                                } else {
                                    "another class"
                                })
                                .or_default() += count - 1;
                        }
                    }
                }

                let objects_by_id: BTreeMap<u32, u16> = objects
                    .iter()
                    .map(|object| (object.object_id, object.class_index))
                    .collect();

                let loop_first_edge: BTreeMap<u32, u32> = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.edge_loop && object.identifiers.len() == 3
                    })
                    .map(|object| (object.object_id, object.identifiers[1]))
                    .collect();

                // `Edge.identifiers` is `[pFace0, pFace1, next0, next1,
                // prev0, prev1]`, so the ring around face `pFace[side]`
                // continues at `next[side]`.
                let edge_links: BTreeMap<u32, [u32; 4]> = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.edge && object.identifiers.len() == 6
                    })
                    .map(|edge| {
                        (
                            edge.object_id,
                            [
                                edge.identifiers[0],
                                edge.identifiers[1],
                                edge.identifiers[2],
                                edge.identifiers[3],
                            ],
                        )
                    })
                    .collect();
                let mut edges_of_face: BTreeMap<u32, Vec<(u32, usize)>> = BTreeMap::new();
                for (id, links) in &edge_links {
                    for (side, face) in links[..2].iter().enumerate() {
                        if *face != 0 {
                            edges_of_face.entry(*face).or_default().push((*id, side));
                        }
                    }
                }

                for face in objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                {
                    let has_a_loop = face
                        .references
                        .first()
                        .is_some_and(|reference| reference.object_id != 0);
                    let claiming = claimed.get(&face.object_id).map_or(&[][..], Vec::as_slice);
                    let verdict = match rebuild_rings(
                        face.object_id,
                        edges_of_face
                            .get(&face.object_id)
                            .map_or(&[][..], Vec::as_slice),
                        &edge_links,
                    ) {
                        Err(failure) => {
                            rebuilt_tally[usize::from(has_a_loop)]
                                [usize::from(failure.starts_with("no edge"))] += 1;
                            failure
                        }
                        Ok(rebuilt) => {
                            // Each ring ends by stepping onto the `EdgeLoop`
                            // that owns it, and each loop declares the edge
                            // its ring starts at, so agreement is checkable in
                            // both directions rather than by counting.
                            for (_, terminator, length) in &rebuilt {
                                *ring_lengths[usize::from(claiming.contains(terminator))]
                                    .entry(*length)
                                    .or_default() += 1;
                                if claiming.contains(terminator) {
                                    continue;
                                }
                                *terminator_range
                                    .entry(if *terminator == 0 {
                                        "a null link"
                                    } else if objects_by_id.contains_key(terminator) {
                                        "an object of this record"
                                    } else if *terminator == u32::MAX {
                                        "0xffffffff"
                                    } else if objects_by_id
                                        .keys()
                                        .next_back()
                                        .is_some_and(|highest| terminator <= highest)
                                    {
                                        "an unused identifier below the record's highest"
                                    } else {
                                        "above every identifier the record uses"
                                    })
                                    .or_default() += 1;
                                *terminators
                                    .entry(objects_by_id.get(terminator).map_or_else(
                                        || "no object of this record".to_owned(),
                                        |class_index| {
                                            schema.class_by_index(*class_index).map_or_else(
                                                || format!("class {class_index}"),
                                                |class| class.name.clone(),
                                            )
                                        },
                                    ))
                                    .or_default() += 1;
                            }
                            rebuilt_tally[usize::from(has_a_loop)][2] += 1;
                            rings_beyond_the_loops[usize::from(has_a_loop)] +=
                                (rebuilt.len() as u64).saturating_sub(claiming.len() as u64);
                            let ends_on_its_loop = rebuilt
                                .iter()
                                .all(|(_, terminator, _)| claiming.contains(terminator));
                            let starts_where_the_loops_say = claiming.iter().all(|loop_id| {
                                loop_first_edge.get(loop_id).is_some_and(|first| {
                                    rebuilt.iter().any(|(head, _, _)| head == first)
                                })
                            });
                            format!(
                                "{} ring(s) against {} loop(s); ends on its loop: {}; starts \
                                 where the loops say: {}",
                                rebuilt.len(),
                                claiming.len(),
                                ends_on_its_loop,
                                starts_where_the_loops_say,
                            )
                        }
                    };
                    *rings[usize::from(has_a_loop)].entry(verdict).or_default() += 1;
                }

                let loopless: BTreeSet<u32> = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.face
                            && object
                                .references
                                .first()
                                .is_none_or(|reference| reference.object_id == 0)
                    })
                    .map(|object| object.object_id)
                    .collect();
                for edge in objects
                    .iter()
                    .filter(|object| object.class_index == classes.edge)
                {
                    let named = &edge.identifiers.get(..2).unwrap_or_default();
                    let verdict = match named.iter().filter(|face| loopless.contains(face)).count()
                    {
                        0 if named.len() == 2 => "both faces bounded",
                        1 => "one face has no loop",
                        2 => "both faces have no loop",
                        _ => "edge names fewer than two faces",
                    };
                    *edges_naming.entry(verdict).or_default() += 1;
                }

                let record_faces = objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                    .count();
                let record_null = objects
                    .iter()
                    .filter(|object| {
                        object.class_index == classes.face
                            && object
                                .references
                                .first()
                                .is_none_or(|reference| reference.object_id == 0)
                    })
                    .count();
                *record_split
                    .entry(if record_null == 0 {
                        "every face reaches a loop"
                    } else if record_null == record_faces {
                        "no face in the record reaches a loop"
                    } else {
                        "some faces reach a loop and some do not"
                    })
                    .or_default() += 1;

                if record_null > 0 {
                    short_records += 1;
                    let all_have_edges = objects
                        .iter()
                        .filter(|object| {
                            object.class_index == classes.face
                                && object
                                    .references
                                    .first()
                                    .is_none_or(|reference| reference.object_id == 0)
                        })
                        .all(|face| edges_of_face.contains_key(&face.object_id));
                    every_loopless_face_has_edges += u64::from(all_have_edges);
                    // Whether the loop is the record's *only* complaint. A
                    // record excluded for something else as well is not
                    // completed by ordering edges.
                    let only_the_loop =
                        rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes)
                            .excluded_faces
                            .iter()
                            .all(|exclusion| exclusion.reason == "face has no first loop");
                    short_only_for_want_of_a_loop += u64::from(only_the_loop);
                    recoverable_records += u64::from(all_have_edges && only_the_loop);
                }

                // The same question asked of the exclusions themselves, which
                // is what a fix would actually buy: a face excluded for
                // carrying no first loop has already resolved its surface,
                // because `assemble_face` reads that first.
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes);
                for exclusion in &assembled.excluded_faces {
                    if exclusion.reason != "face has no first loop" {
                        continue;
                    }
                    bump(&mut excluded_without_a_loop);
                    if claimed
                        .get(&exclusion.face_id)
                        .is_some_and(|loops| loops.len() == 1)
                    {
                        bump(&mut recoverable);
                    }
                }
            }
        },
    )?;

    println!("Faces in face-bearing `GElement` records: {faces:?} (all, exactly tiled)");
    println!("  carrying a usable first-loop reference: {referenced:?}");
    println!("\nControl - what `EdgeLoop.pFace` says about a face that carries a reference:");
    print_loop_owner_rows(&control, rows);
    println!("\nWhat it says about a face that carries none:");
    print_loop_owner_rows(&unreferenced, rows);
    println!("\nWhat those faces carry in the first reference slot instead:");
    let mut shapes = first_reference.into_iter().collect::<Vec<_>>();
    shapes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (shape, count) in shapes.into_iter().take(rows) {
        println!("  {count}\t{}", escape_terminal_text(&shape));
    }
    println!("\n  further references such a face carries (count: faces):");
    for (count, faces) in &reference_counts {
        println!("  {count}\t{faces}");
    }
    println!(
        "\nRecords carrying a loopless face: {short_records}, of them every loopless face \
         named by an edge: {every_loopless_face_has_edges}, excluded for nothing else: \
         {short_only_for_want_of_a_loop}, both: {recoverable_records}"
    );
    println!(
        "\nRebuild in three numbers (rebuild failed, no edge names the face, rings recovered):\n  \
         a face with a loop: {:?}, rings beyond the loops the record holds: {}\n  \
         a face with none:   {:?}, rings beyond the loops the record holds: {}",
        rebuilt_tally[1], rings_beyond_the_loops[1], rebuilt_tally[0], rings_beyond_the_loops[0]
    );
    println!("\nEdges per ring - a ring an `EdgeLoop` of the record ends:");
    print_ring_lengths(&ring_lengths[1]);
    println!("  and a ring no loop of the record knows about:");
    print_ring_lengths(&ring_lengths[0]);
    println!("\nWhat kind of identifier such a ring ends on:");
    for (kind, count) in &terminator_range {
        println!("  {count}\t{kind}");
    }
    println!("\nWhere a ring ends when it is not on a loop that claims the face:");
    print_scalar_rows(&terminators, rows);
    println!("\nObjects sharing an identifier with another of their class in one record:");
    for (class, count) in &duplicate_identifiers {
        println!("  {count}\t{class}");
    }
    println!("\nRebuilding a face's boundary from the edges alone - a face with a loop:");
    print_scalar_rows(&rings[1], rows);
    println!("  and a face with none:");
    print_scalar_rows(&rings[0], rows);
    println!("\nWhat the edges say about the two kinds of face:");
    let mut naming = edges_naming.into_iter().collect::<Vec<_>>();
    naming.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in naming {
        println!("  {count}\t{verdict}");
    }
    println!("\n  what names such a face:");
    print_scalar_rows(&null_parents, rows);
    println!("\n  and what names a loop-bearing face:");
    print_scalar_rows(&loop_bearing_parents, rows);
    println!("\n  the scalars such a face declares:");
    print_scalar_rows(&null_scalars, rows);
    println!("\n  and those a loop-bearing face declares:");
    print_scalar_rows(&loop_bearing_scalars, rows);
    println!("  references a loop-bearing face carries (count: faces):");
    for (count, faces) in &loop_bearing_reference_counts {
        println!("  {count}\t{faces}");
    }
    println!("\n  the surface such a face names:");
    let mut surfaces = null_surface_class.into_iter().collect::<Vec<_>>();
    surfaces.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (class, count) in surfaces {
        println!("  {count}\t{}", escape_terminal_text(&class));
    }
    println!(
        "  sharing that surface with a loop-bearing face of the same record: {shares_a_surface}"
    );
    println!("  whose own identifier a loop-bearing face also carries: {shares_an_identifier}");
    println!("\nHow the two kinds of face fall across records:");
    let mut split = record_split.into_iter().collect::<Vec<_>>();
    split.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in split {
        println!("  {count}\t{verdict}");
    }
    println!("\nWhat each `GBRep` node holds, by whether an edge reaches its faces:");
    let mut kinds = node_kind.into_iter().collect::<Vec<_>>();
    kinds.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (kind, count) in kinds {
        let faces = node_faces_by_kind.get(kind).copied().unwrap_or_default();
        println!("  {count}\tnodes, {faces} faces\t{kind}");
    }
    println!("  faces per node, for the nodes no edge reaches (faces: nodes):");
    for (faces, nodes) in &free_node_faces {
        println!("  {faces}\t{nodes}");
    }
    println!("  such nodes per record (nodes: records):");
    for (nodes, records) in &free_nodes_per_record {
        println!("  {nodes}\t{records}");
    }
    println!("\n  what a face with no loop, but with edges, is ordered by:");
    let mut links = loopless_next_links.into_iter().collect::<Vec<_>>();
    links.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in links {
        println!("  {count}\t{verdict}");
    }
    println!("\n  the marks a face with no loop and no edge naming it carries:");
    print_scalar_rows(&loopless_flags, rows);
    println!("  and those every other face carries:");
    print_scalar_rows(&loop_bearing_flags, rows);
    println!("\n  where a face with no loop and no edge naming it sits:");
    let mut placed = loopless_by_node.into_iter().collect::<Vec<_>>();
    placed.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (verdict, count) in placed {
        println!("  {count}\t{verdict}");
    }
    println!(
        "\nFaces excluded as \"face has no first loop\": {excluded_without_a_loop:?}, \
         of which exactly one loop claims: {recoverable:?}"
    );
    Ok(())
}

/// Ring sizes, smallest first, with everything past ten collapsed.
pub(crate) fn print_ring_lengths(rows: &BTreeMap<usize, u64>) {
    let mut long = 0_u64;
    for (length, count) in rows {
        if *length > 10 {
            long += count;
        } else {
            println!("  {length} edge(s)\t{count}");
        }
    }
    println!("  11+ edges\t{long}");
}

/// Walk the edges that name one face into rings, and say what came of it.
///
/// From any edge, the next edge around this face is `next[side]`, and the ring
/// closes by naming something that is not an edge of this record - the
/// `EdgeLoop` object, which is the one thing a loopless face does not have.
/// So the ring is walked until it leaves the edges, and the verdict is
/// whether the rings partition exactly the edges that name the face.
pub(crate) fn rebuild_rings(
    face_id: u32,
    edges: &[(u32, usize)],
    links: &BTreeMap<u32, [u32; 4]>,
) -> Result<Vec<(u32, u32, usize)>, String> {
    if edges.is_empty() {
        return Err("no edge names the face".to_owned());
    }
    // The chain a ring makes is a path, not a cycle: its last edge's next
    // names the `EdgeLoop` object rather than the first edge. So a ring has to
    // be walked from its head - the edge no other edge of this face steps onto
    // - and starting anywhere else would walk only a suffix.
    let successors: BTreeSet<u32> = edges
        .iter()
        .filter_map(|&(edge, side)| {
            let next = links[&edge][2 + side];
            links.contains_key(&next).then_some(next)
        })
        .collect();
    let heads = edges
        .iter()
        .filter(|(edge, _)| !successors.contains(edge))
        .copied()
        .collect::<Vec<_>>();
    if heads.is_empty() {
        return Err("no edge of the face begins a ring".to_owned());
    }
    let mut visited = BTreeSet::new();
    // Each ring as the edge it starts at and the identifier its last edge
    // steps onto - which should be the `EdgeLoop` that names this face.
    let mut rings = Vec::new();
    for &(start, start_side) in &heads {
        if visited.contains(&start) {
            continue;
        }
        let before = visited.len();
        let (mut edge, mut side) = (start, start_side);
        let terminator = loop {
            if !visited.insert(edge) {
                return Err("the ring came back to an edge it had already used".to_owned());
            }
            let next = links[&edge][2 + side];
            let Some(next_links) = links.get(&next) else {
                break next;
            };
            side = if next_links[0] == face_id {
                0
            } else if next_links[1] == face_id {
                1
            } else {
                return Err("the ring stepped onto an edge of another face".to_owned());
            };
            edge = next;
        };
        rings.push((start, terminator, visited.len() - before));
    }
    if visited.len() != edges.len() {
        return Err("the rings did not use every edge that names the face".to_owned());
    }
    Ok(rings)
}

/// A face's declared scalars, as one comparable key: the `Integer32` fields
/// `m_cutType` and `m_faceFlags_v9`, then whatever narrower fields the walk
/// read.
pub(crate) fn face_scalars(face: &rvt_model::SerialObject) -> String {
    format!("{:?} / {:?}", face.integers, face.small_integers)
}

/// One histogram of face scalars, most frequent first.
pub(crate) fn print_scalar_rows(rows: &BTreeMap<String, u64>, limit: usize) {
    let mut ordered = rows.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    for (scalars, count) in ordered.into_iter().take(limit) {
        println!("  {count}\t{}", escape_terminal_text(scalars));
    }
}

/// One tally of the loop-owner probe, most frequent first, as `count (exact:
/// count)`.
pub(crate) fn print_loop_owner_rows(rows: &BTreeMap<&'static str, [u64; 2]>, limit: usize) {
    let mut ordered = rows.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1[0].cmp(&left.1[0]).then_with(|| left.0.cmp(right.0)));
    for (verdict, counts) in ordered.into_iter().take(limit) {
        println!("  {}\t(exact: {})\t{verdict}", counts[0], counts[1]);
    }
}

/// Face-count buckets for the openness breakdown, as `(name, largest count in
/// it)`. The single digits are called out on their own because they are the
/// shapes that say what a record is: one face is a plane, and six is the face
/// count of a box.
pub(crate) const FACE_BUCKETS: [(&str, usize); 7] = [
    ("1", 1),
    ("2", 2),
    ("3-5", 5),
    ("6", 6),
    ("7-12", 12),
    ("13-50", 50),
    ("51+", usize::MAX),
];

/// Which [`FACE_BUCKETS`] entry a face count falls in.
pub(crate) fn face_bucket(faces: usize) -> usize {
    FACE_BUCKETS
        .iter()
        .position(|(_, largest)| faces <= *largest)
        .unwrap_or(FACE_BUCKETS.len() - 1)
}

/// Tally what the boundary-representation assembly gets and what it drops,
/// over every record of a file.
///
/// The bar for a geometry rule is not that a hand-picked record improves: it is
/// that the corpus-wide face and body counts rise with no new exclusion reason
/// appearing. This reports exactly those, so a change can be diffed rather than
/// argued, and it is the cheap half of the check - the other half is
/// `ifcopenshell.geom.create_shape` on every emitted body.
#[allow(clippy::too_many_lines)] // One streaming pass plus its report.
pub(crate) fn brep(
    path: &Path,
    reasons: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let classes = brep_class_indexes(schema.as_ref());
    let body_classes = brep_body_classes(schema.as_ref());
    // The solid lives in a `GElement` record, and only there. Reading every
    // record that merely contains a `Face` object instead would tally tens of
    // thousands of truncated node streams as exclusions and measure nothing.
    let geometry_element_class_index = schema_class_index(schema.as_ref(), "GElement");
    let (Some(schema), Some(classes), Some(geometry_element_class_index)) =
        (schema.as_ref(), classes, geometry_element_class_index)
    else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare the boundary-representation classes",
        )));
    };
    let partition_paths = partition_paths(&container);

    let mut records = 0_u64;
    let mut complete = 0_u64;
    let mut faces = 0_u64;
    let mut excluded = 0_u64;
    let mut loops = 0_u64;
    let mut edges = 0_u64;
    let mut arcs = 0_u64;
    let mut polylines = 0_u64;
    let mut why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut exact_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut edge_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut exact_edge_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut ordering_control = rvt_model::OrderingControl::default();
    let mut holes = rvt_model::HoleTally::default();
    let mut hole_why: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut gaps: Vec<f64> = Vec::new();
    let mut gaps_with_a_cylinder = 0_u64;
    let mut gaps_in_exact_records = 0_u64;
    // Faces the record declares that are on no boundary of it - see
    // `SymbolBrep::unbounded_faces`. Reported, because "not a failure" is not
    // the same as "not there", and a change that started counting them as
    // faces again should be visible here rather than in a shortfall elsewhere.
    let mut unbounded = 0_u64;
    let mut records_with_unbounded = 0_u64;
    // Of the records that excluded no face, how many hold faces that actually
    // close - the geometric test an exporter applies, against the topological
    // one this tally is otherwise about. The two are far apart and the gap is
    // worth having in front of whoever reads this next.
    let mut complete_and_a_volume = 0_u64;
    // And of the ones that do not, what their bodies say about why - see
    // `rvt_model::BrepOpenness`. A record is not one body, so most of this gap
    // is expected to be records carrying a solid *and* something beside it;
    // the point of the breakdown is to say how much of it is that and how much
    // is a shell with a face missing from it.
    let mut openness: BTreeMap<rvt_model::BrepOpenness, u64> = BTreeMap::new();
    let mut openness_faces: BTreeMap<rvt_model::BrepOpenness, [u64; FACE_BUCKETS.len()]> =
        BTreeMap::new();
    // Of each class, the records holding a face some of whose edges no loop of
    // theirs uses. That is a hole nobody read, and it is the one reading that
    // would leave a body closed on its edges and open on its loops without
    // anything being wrong with the edges themselves - so it separates the two
    // halves of `BrepOpenness::ClosedOnItsEdgesOnly` rather than leaving them
    // named together.
    let mut openness_short: BTreeMap<rvt_model::BrepOpenness, u64> = BTreeMap::new();
    // And of each class, the records that close topologically all the same.
    // That is the other half of the exporter's own test
    // (`is_closed() || bounds_a_volume()`), so it says which of these classes
    // is already reaching the export and which is refused outright.
    let mut openness_closed: BTreeMap<rvt_model::BrepOpenness, u64> = BTreeMap::new();
    let mut one_sided_edges = 0_u64;
    let mut edges_out_of_their_body = 0_u64;
    let mut exact_records = 0_u64;
    let mut exact_complete = 0_u64;
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (record_walk, objects) =
                    rvt_model::walk_record_collecting(schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == classes.face)
                {
                    continue;
                }
                // Whether the record's declarations tiled its body exactly. A
                // walk that drifted hands this module bytes that are not the
                // fields it thinks they are, so every number after the drift is
                // arbitrary - which is a different failure from a geometry rule
                // being wrong, and has to be counted apart from one.
                let exact = record_walk.is_exact();
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes);
                unbounded += assembled.unbounded_faces.len() as u64;
                records_with_unbounded += u64::from(!assembled.unbounded_faces.is_empty());
                if assembled.is_empty() && assembled.excluded_faces.is_empty() {
                    continue;
                }
                records += 1;
                exact_records += u64::from(exact);
                complete += u64::from(assembled.excluded_faces.is_empty());
                if assembled.excluded_faces.is_empty() {
                    if assembled.bounds_a_volume() {
                        complete_and_a_volume += 1;
                    } else {
                        let reading = assembled.openness();
                        *openness.entry(reading).or_default() += 1;
                        openness_faces.entry(reading).or_default()
                            [face_bucket(assembled.faces.len())] += 1;
                        *openness_short.entry(reading).or_default() +=
                            u64::from(assembled.holes.edges_short > 0);
                        *openness_closed.entry(reading).or_default() +=
                            u64::from(assembled.is_closed());
                        for body in &assembled.bodies {
                            one_sided_edges += body.one_sided_edges as u64;
                            edges_out_of_their_body += body.open_edges as u64;
                        }
                    }
                }
                exact_complete += u64::from(exact && assembled.excluded_faces.is_empty());
                faces += assembled.faces.len() as u64;
                excluded += assembled.excluded_faces.len() as u64;
                for face in &assembled.faces {
                    loops += face.loops.len() as u64;
                    for face_loop in &face.loops {
                        edges += face_loop.len() as u64;
                        arcs += face_loop
                            .iter()
                            .filter(|edge| matches!(edge.curve, rvt_model::BrepCurve::Arc(_)))
                            .count() as u64;
                        polylines += face_loop
                            .iter()
                            .filter(|edge| matches!(edge.curve, rvt_model::BrepCurve::Polyline(_)))
                            .count() as u64;
                    }
                }
                for exclusion in &assembled.excluded_faces {
                    *why.entry(exclusion.reason).or_default() += 1;
                    if exact {
                        *exact_why.entry(exclusion.reason).or_default() += 1;
                    }
                }
                ordering_control.agreed += assembled.ordering_control.agreed;
                ordering_control.refused += assembled.ordering_control.refused;
                ordering_control.contradicted += assembled.ordering_control.contradicted;
                ordering_control.not_comparable += assembled.ordering_control.not_comparable;
                holes.faces += assembled.holes.faces;
                holes.loops += assembled.holes.loops;
                holes.edges_accounted += assembled.holes.edges_accounted;
                holes.first_loop_accounted += assembled.holes.first_loop_accounted;
                holes.edges_short += assembled.holes.edges_short;
                holes.edges_over += assembled.holes.edges_over;
                for unread in &assembled.holes.unread {
                    *hole_why.entry(unread.reason).or_default() += 1;
                }
                // Whether a record holds any cylinder at all is the control for
                // the one place a face's surface is *inferred* rather than
                // read: `CylSurf` objects share one identifier, so they are
                // paired to faces by encounter order. If that pairing slips, a
                // face is evaluated against a surface that is not its own -
                // which is exactly what a cross-face disagreement looks like.
                // A record with no cylinder cannot suffer from it.
                let cylindrical =
                    assembled.faces.iter().any(|face| {
                        matches!(face.surface, rvt_model::BrepSurface::Cylinder { .. })
                    }) || objects
                        .iter()
                        .any(|object| object.class_index == classes.cyl_surf);
                for failure in &assembled.failed_edges {
                    *edge_why.entry(failure.reason).or_default() += 1;
                    if exact {
                        *exact_edge_why.entry(failure.reason).or_default() += 1;
                    }
                    if let Some(gap) = failure.gap_feet {
                        gaps.push(gap);
                        if cylindrical {
                            gaps_with_a_cylinder += 1;
                        }
                        if exact {
                            gaps_in_exact_records += 1;
                        }
                    }
                }
            }
        },
    )?;

    println!("Boundary representations assembled from face-bearing records:");
    println!("Records producing a body: {records}");
    println!("  whose declarations tiled the body exactly: {exact_records}");
    println!("  every face resolved: {complete}");
    println!("    of those, in an exactly-tiled record: {exact_complete}");
    println!("  complete records bounding a volume: {complete_and_a_volume}");
    let not_a_volume = complete - complete_and_a_volume;
    // The backlog proper: a record whose best body neither bounds a volume nor
    // pairs its curves up is one nothing here can hand an exporter, and it is
    // the only one of the classes below that a further reading could move.
    let without_a_closed_body = openness
        .iter()
        .filter(|(reading, _)| {
            !matches!(
                reading,
                rvt_model::BrepOpenness::BoundsAVolume
                    | rvt_model::BrepOpenness::ClosesCurveForCurve
            )
        })
        .map(|(_, count)| *count)
        .sum::<u64>();
    println!("  complete records that do not bound a volume: {not_a_volume}");
    println!("    of those, none of their bodies closes at all: {without_a_closed_body}");
    for (reading, count) in &openness {
        println!("    {count}\t{}", reading.label());
        let mut row = String::new();
        for (index, (name, _)) in FACE_BUCKETS.iter().enumerate() {
            let faces = openness_faces
                .get(reading)
                .map_or(0, |buckets| buckets[index]);
            let _ = write!(row, " {name}:{faces}");
        }
        println!("      faces per record:{row}");
        println!(
            "      of them, holding a face whose loops leave an edge over: {}, \
             closing on their edges: {}",
            openness_short.get(reading).copied().unwrap_or(0),
            openness_closed.get(reading).copied().unwrap_or(0)
        );
    }
    println!(
        "    edges with nothing on the far side: {one_sided_edges}, \
         edges naming a face outside their body: {edges_out_of_their_body}"
    );
    println!(
        "  faces on no boundary of their record: {unbounded}, in {records_with_unbounded} records"
    );
    println!("Faces resolved: {faces}");
    println!("  loops: {loops}, edges: {edges}, of which arcs: {arcs}, polylines: {polylines}");
    println!(
        "Faces excluded: {excluded} (in an exactly-tiled record: {})",
        exact_why.values().sum::<u64>()
    );
    let mut ordered = why.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (reason, count) in ordered.into_iter().take(reasons) {
        println!(
            "  {count}\t(exact: {})\t{}",
            exact_why.get(reason).copied().unwrap_or(0),
            escape_terminal_text(reason)
        );
    }

    // The licence for the reconstruction a loopless face is assembled by:
    // where the file declares the ring, ordering the edges by their endpoints
    // has to find the same one.
    println!(
        "Ordering a declared loop's edges by their endpoints: {} agreed, {} refused, \
         {} contradicted, {} not comparable",
        ordering_control.agreed,
        ordering_control.refused,
        ordering_control.contradicted,
        ordering_control.not_comparable
    );

    // A face's holes, and the check on them that does not come from the loop
    // chain at all: the edges name their face from their own side, so a face
    // whose loops use exactly those edges has had its whole boundary read.
    println!(
        "Faces carrying a further loop: {}, further loops read: {}",
        holes.faces, holes.loops
    );
    println!(
        "  resolved faces whose loops use exactly the edges naming them: {} (by the first loop alone: {}), {} leave edges over, {} use more",
        holes.edges_accounted, holes.first_loop_accounted, holes.edges_short, holes.edges_over
    );
    let mut ordered = hole_why.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (reason, count) in ordered.into_iter().take(reasons) {
        println!(
            "  {count}\tloop chain stopped: {}",
            escape_terminal_text(reason)
        );
    }

    // Edges, not faces. A face is excluded by the first failing edge its loop
    // reaches, so face counts say how much was lost and these say how much is
    // actually wrong.
    println!(
        "Edges that did not resolve: {} (in an exactly-tiled record: {})",
        edge_why.values().sum::<u64>(),
        exact_edge_why.values().sum::<u64>()
    );
    let mut ordered = edge_why.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    for (reason, count) in ordered.into_iter().take(reasons) {
        println!(
            "  {count}\t(exact: {})\t{}",
            exact_edge_why.get(reason).copied().unwrap_or(0),
            escape_terminal_text(reason)
        );
    }

    if !gaps.is_empty() {
        // How far apart the two faces put a shared EdgePnt separates a
        // tolerance that is too tight from a face evaluated against the wrong
        // surface. Reported in millimetres, which is the scale the answer
        // matters at - except that a non-finite gap is neither: it means an
        // evaluated point is infinite or NaN, so some number feeding it is not
        // a coordinate at all, and the two are counted apart.
        let total = gaps.len();
        let non_finite = gaps.iter().filter(|gap| !gap.is_finite()).count();
        let share = |part: u64| {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / total as f64
            }
        };
        gaps.retain(|gap| gap.is_finite());
        gaps.sort_by(f64::total_cmp);
        let millimetres = |feet: f64| feet * 304.8;
        println!("Cross-face EdgePnt disagreements: {total}");
        println!(
            "  in a record that holds a cylinder: {gaps_with_a_cylinder} ({:.1}%)",
            share(gaps_with_a_cylinder)
        );
        println!(
            "  in a record whose declarations tiled it exactly: {gaps_in_exact_records} ({:.1}%)",
            share(gaps_in_exact_records)
        );
        println!(
            "  the evaluated point is infinite or NaN: {non_finite} ({:.1}%)",
            share(non_finite as u64)
        );
        if !gaps.is_empty() {
            for (label, bound_mm) in [
                ("under 0.01 mm", 0.01),
                ("under 0.1 mm", 0.1),
                ("under 1 mm", 1.0),
                ("under 10 mm", 10.0),
                ("under 1 m", 1000.0),
            ] {
                let count = gaps
                    .iter()
                    .filter(|gap| millimetres(**gap) < bound_mm)
                    .count();
                println!("  finite and {label}: {count}");
            }
            let quantile = |q: f64| {
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let index = ((gaps.len() - 1) as f64 * q).round() as usize;
                millimetres(gaps[index])
            };
            println!(
                "  finite: median {:.4} mm, 90th {:.4} mm, max {:.4} mm",
                quantile(0.5),
                quantile(0.9),
                millimetres(*gaps.last().unwrap_or(&0.0))
            );
        }
    }
    Ok(())
}

/// Millimetres in one Revit internal foot.
pub(crate) const MILLIMETRES_PER_FOOT: f64 = 304.8;

/// What one candidate type-reference property named, for one class that
/// declares it: how many values it carried, what classes they named, and how
/// many distinct elements they named between them.
#[derive(Default)]
pub(crate) struct TypeLinkTally<'a> {
    pub(crate) values: u64,
    pub(crate) named_classes: BTreeMap<&'a str, u64>,
    pub(crate) named_elements: BTreeSet<i32>,
}

/// Report the layer table compound host object types carry, and score every
/// candidate type-reference property by the class of the element it names.
///
/// Both halves are measurements rather than output. A type reference is
/// credible when its values land on one class out of the file's thousands, and
/// a layer table is credible when its widths and materials reproduce an answer
/// this decode did not produce: Revit's own IFC export of the same model
/// carries an `IfcMaterialConstituentSet` per wall, with the layers in order,
/// the material names, and each layer's share of the total width. `--links`
/// prints the element-to-type pairs so that join can be made.
pub(crate) fn layers(
    path: &Path,
    types: usize,
    links_wanted: bool,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let recovered = recover_elements(path, max_member_bytes)?;
    let schema = recovered.schema.as_ref();
    let elements = &recovered.elements;
    let class_of = |id: u32| -> Option<&str> {
        elements
            .get(&id)
            .and_then(|element| element_class_name(element, schema))
    };
    let name_of = |id: u32| -> Option<&str> {
        elements
            .get(&id)
            .and_then(|element| element.name.as_ref())
            .map(|(name, _)| name.as_str())
    };

    report_type_links(elements, schema, &class_of);
    if links_wanted {
        println!();
        for (id, element) in elements {
            let (property, target) = match (element.type_element_property, element.type_element_id)
            {
                (Some(property), Some(target)) => (property, target),
                // A record with no type reference still reports its family and
                // its category, which is the other half of the same join.
                _ if element.family_element_id.is_some()
                    || element.declared_category_id.is_some() =>
                {
                    ("-", -1)
                }
                _ => continue,
            };
            let target_id = u32::try_from(target).ok();
            println!(
                "LINK {id}\t{owner}\t{property}\t{target}\t{class}\t{name}\t{family}\t{category}",
                owner = element_class_name(element, schema).unwrap_or("-"),
                class = target_id.and_then(&class_of).unwrap_or("-"),
                name = target_id.and_then(&name_of).unwrap_or("-"),
                family = element.family_element_id.unwrap_or(-1),
                category = element.declared_category_id.unwrap_or(-1),
            );
        }
    }
    report_family_categories(elements, schema, &class_of);
    report_layer_carriers(elements, schema);
    report_layer_tables(elements, schema, types, &name_of);
    Ok(())
}

/// The class of the element's own record, when the schema names one.
pub(crate) fn element_class_name<'a>(
    element: &ExportedElement,
    schema: Option<&'a Schema>,
) -> Option<&'a str> {
    element
        .class_index
        .and_then(|index| schema?.class_by_index(index))
        .map(|class| class.name.as_str())
}

/// What each candidate property names, per class that declares it. A real type
/// reference lands on one class out of the file's thousands; a misread field
/// scatters across them.
pub(crate) fn report_type_links<'a>(
    elements: &'a BTreeMap<u32, ExportedElement>,
    schema: Option<&'a Schema>,
    class_of: &impl Fn(u32) -> Option<&'a str>,
) {
    let mut links: BTreeMap<(&str, &str), TypeLinkTally<'a>> = BTreeMap::new();
    for element in elements.values() {
        let (Some(property), Some(target)) =
            (element.type_element_property, element.type_element_id)
        else {
            continue;
        };
        let Some(owner) = element_class_name(element, schema) else {
            continue;
        };
        let row = links.entry((property, owner)).or_default();
        row.values += 1;
        *row.named_classes
            .entry(
                u32::try_from(target)
                    .ok()
                    .and_then(class_of)
                    .unwrap_or("<no such element>"),
            )
            .or_default() += 1;
        row.named_elements.insert(target);
    }
    println!("Type references, by the property that carried them:");
    let mut rows = links.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, row)| std::cmp::Reverse(row.values));
    for ((property, owner), row) in rows {
        let named = row
            .named_classes
            .iter()
            .map(|(class, count)| format!("{class} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {property}\ton {owner}: {values} values naming {distinct} elements - {named}",
            values = row.values,
            distinct = row.named_elements.len(),
        );
    }
}

/// The route a loadable family's category takes: symbol -> family -> category,
/// and what each hop reaches. See `inherit_family_categories`.
pub(crate) fn report_family_categories<'a>(
    elements: &'a BTreeMap<u32, ExportedElement>,
    schema: Option<&'a Schema>,
    class_of: &impl Fn(u32) -> Option<&'a str>,
) {
    let mut families: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    let mut declaring: BTreeMap<&str, u64> = BTreeMap::new();
    let mut sources: BTreeMap<&str, u64> = BTreeMap::new();
    let mut without = 0_u64;
    for element in elements.values() {
        let owner = element_class_name(element, schema).unwrap_or("-");
        if let Some(family) = element.family_element_id {
            let named = u32::try_from(family)
                .ok()
                .and_then(class_of)
                .unwrap_or("<no such element>");
            *families.entry((owner, named)).or_default() += 1;
        }
        if element.declared_category_id.is_some() {
            *declaring.entry(owner).or_default() += 1;
        }
        match element.category_source {
            Some(source) => *sources.entry(source).or_default() += 1,
            None if element.category.is_some() => *sources.entry("-").or_default() += 1,
            None => without += 1,
        }
    }
    println!();
    println!("Family references, by the class that declares one and the class it names:");
    let mut rows = families.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for ((owner, named), count) in rows.into_iter().take(8) {
        println!("  m_familyId	on {owner}: {count} naming {named}");
    }
    println!("Records declaring a category identifier:");
    let mut rows = declaring.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (owner, count) in rows.into_iter().take(8) {
        println!("  m_categoryId	on {owner}: {count}");
    }
    println!("Categories, by where they came from:");
    for (source, count) in sources {
        println!("  {source}: {count}");
    }
    println!("  no category: {without}");
}

/// Every class that can carry a layer table, whether or not one was read, so a
/// class that carries none is visible rather than absent.
pub(crate) fn report_layer_carriers(
    elements: &BTreeMap<u32, ExportedElement>,
    schema: Option<&Schema>,
) {
    let mut carriers: BTreeMap<&str, (u64, u64, BTreeMap<usize, u64>)> = BTreeMap::new();
    for element in elements.values() {
        let Some(class) = element_class_name(element, schema) else {
            continue;
        };
        let host_object_type = element.class_index.is_some_and(|index| {
            schema.is_some_and(|schema| {
                rvt_model::descends_from(
                    schema,
                    index,
                    rvt_model::HOST_OBJECT_ATTRIBUTES_CLASS_NAME,
                )
            })
        });
        if !host_object_type {
            continue;
        }
        let row = carriers.entry(class).or_default();
        row.0 += 1;
        if let Some(structure) = element.compound_structures.first() {
            row.1 += 1;
            *row.2.entry(structure.layers.len()).or_default() += 1;
        }
    }
    println!();
    println!("Layer tables, by the class of the type that carries them:");
    let mut rows = carriers.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(_, row)| std::cmp::Reverse(row.0));
    for (class, (ids, read, histogram)) in rows {
        let counts = histogram
            .iter()
            .map(|(layers, ids)| format!("{layers}:{ids}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!("  {class}\tids={ids}\twith a layer table={read}\tlayers {counts}");
    }
}

/// The tables themselves, most layers first: this is what an independent
/// answer is compared against.
pub(crate) fn report_layer_tables<'a>(
    elements: &'a BTreeMap<u32, ExportedElement>,
    schema: Option<&'a Schema>,
    types: usize,
    name_of: &impl Fn(u32) -> Option<&'a str>,
) {
    let mut listed = elements
        .iter()
        .filter_map(|(id, element)| {
            element
                .compound_structures
                .first()
                .map(|structure| (*id, element, structure))
        })
        .collect::<Vec<_>>();
    listed.sort_by(|left, right| {
        right
            .2
            .layers
            .len()
            .cmp(&left.2.layers.len())
            .then(left.0.cmp(&right.0))
    });
    println!();
    println!(
        "Decoded layer tables: {} of {} shown, widths in millimetres",
        listed.len().min(types),
        listed.len()
    );
    for (id, element, structure) in listed.iter().take(types) {
        println!();
        println!(
            "TYPE {id}\t{class}\t{name}\tlayers={layers}\tcore=[{exterior},{interior}]\ttotal={total:.1}\tpattern={pattern}\tendCap={end_cap}\twrap={wrap}",
            class = element_class_name(element, schema).unwrap_or("-"),
            name = name_of(*id).unwrap_or("-"),
            layers = structure.layers.len(),
            exterior = structure.shell_layers_exterior,
            interior = structure.shell_layers_interior,
            total = structure.total_width_feet() * MILLIMETRES_PER_FOOT,
            pattern = structure.coarse_scale_fill_pattern_id.unwrap_or(-1),
            end_cap = structure.end_cap,
            wrap = structure.opening_wrapping,
        );
        for (index, layer) in structure.layers.iter().enumerate() {
            // Its own column: a marker appended to the material name would be
            // read back as part of the name by anything joining this report
            // against another answer.
            let structural = if structure.structural_layer_index == Some(index) {
                "\tstructural"
            } else {
                ""
            };
            println!(
                "  LAYER {index}\t{width:.1}\tfunction={function}\tmaterial={material_id}\t{material}{structural}",
                width = layer.width_feet * MILLIMETRES_PER_FOOT,
                function = layer.function,
                material_id = layer.material_id.unwrap_or(-1),
                material = layer
                    .material_id
                    .and_then(|id| u32::try_from(id).ok())
                    .and_then(name_of)
                    .unwrap_or("-"),
            );
        }
    }
}

pub(crate) fn names(
    path: &Path,
    classes: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let partition_paths = partition_paths(&container);
    let (calibrations, _) = calibrate_names(
        &container,
        schema.as_ref(),
        &partition_paths,
        max_member_bytes,
    )?;

    let mut ordered = calibrations.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        right
            .1
            .bodies
            .cmp(&left.1.bodies)
            .then_with(|| left.0.cmp(right.0))
    });

    println!("String-offset calibration:");
    println!(
        "A class is accepted when at least {NAME_OFFSET_AGREEMENT}% of its records place their first readable string at the same offset after the Element tail."
    );
    println!();
    let mut settled = 0_usize;
    for (index, calibration) in ordered.iter().take(classes) {
        let name = schema
            .as_ref()
            .and_then(|schema| schema.class_by_index(**index))
            .map_or("?", |class| class.name.as_str());
        let offset = calibration
            .settled_offset()
            .map_or_else(|| "-".to_owned(), |offset| format!("+{offset}"));
        println!(
            "{index}\t{}\tbodies={}\toffset={offset}\tagreement={}%\tsamples={:?}",
            escape_terminal_text(name),
            calibration.bodies,
            calibration.agreement(),
            calibration.samples
        );
    }
    for calibration in calibrations.values() {
        settled += usize::from(calibration.settled_offset().is_some());
    }
    println!();
    println!("Classes seen: {}", calibrations.len());
    println!("Classes with a settled string offset: {settled}");

    let Some(schema) = schema.as_ref() else {
        return Ok(());
    };
    report_declared_names(
        &container,
        schema,
        &partition_paths,
        &calibrations,
        &ordered,
        classes,
        max_member_bytes,
    )
}

/// Measure the name the declarations give a record against the calibrated
/// scan it replaces. The declarations say which property a string is the value
/// of, so a name needs no offset agreement to be located.
pub(crate) fn report_declared_names(
    container: &RvtContainer,
    schema: &Schema,
    partition_paths: &[String],
    calibrations: &BTreeMap<u16, NameCalibration>,
    ordered: &[(&u16, &NameCalibration)],
    classes: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let mut declared: BTreeMap<u16, DeclaredNameTally> = BTreeMap::new();
    for_each_member(
        container,
        partition_paths,
        max_member_bytes,
        |_, _, format_tag, layout, walk, payload| {
            if format_tag != ELEMENT_CLASS_FORMAT_TAG {
                return;
            }
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if !calibrations.contains_key(&header.class_index) {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let tally = declared.entry(header.class_index).or_default();
                tally.bodies += 1;
                // The reader itself answers, so the report cannot drift from
                // what the export actually uses.
                let Some(first) = rvt_model::record_name_string(schema, header.class_index, body)
                else {
                    continue;
                };
                tally.with_string += 1;
                *tally
                    .properties
                    .entry(format!("{}.{}", first.class, first.property))
                    .or_default() += 1;
                let scanned = ElementFields::parse(body, header.id).and_then(|fields| {
                    read_name(
                        body,
                        fields.id_offset + 4 + ELEMENT_TAIL_BYTES,
                        calibrations
                            .get(&header.class_index)
                            .and_then(NameCalibration::settled_offset),
                    )
                });
                if let Some((scanned, _)) = scanned {
                    tally.comparable += 1;
                    if scanned == first.value {
                        tally.agreed += 1;
                    } else {
                        // A disagreement is only evidence once it says what
                        // the scan was reading instead. Name the declaration
                        // whose value the scan returned, so "the scan is one
                        // string early" can be told apart from "the two
                        // readings found unrelated bytes".
                        let (_walk, strings) =
                            rvt_model::walk_record_strings(schema, header.class_index, body);
                        let matched = strings
                            .iter()
                            .find(|string| string.value == scanned)
                            .map_or_else(
                                || "(no declared string)".to_owned(),
                                |string| format!("{}.{}", string.class, string.property),
                            );
                        *tally.scanned_properties.entry(matched).or_default() += 1;
                    }
                }
                if tally.samples.len() < 3 {
                    tally.samples.push(first.value.clone());
                }
            }
        },
    )?;

    println!();
    println!("The name a record's declarations give it, by class:");
    for (index, _) in ordered.iter().take(classes) {
        let Some(tally) = declared.get(index) else {
            continue;
        };
        let name = schema
            .class_by_index(**index)
            .map_or("?", |class| class.name.as_str());
        let property = tally
            .properties
            .iter()
            .max_by_key(|(_, count)| **count)
            .map_or_else(|| "-".to_owned(), |(property, _)| property.clone());
        println!(
            "{index}\t{}\tbodies={}\twith a string={}\tproperty={}\tagrees with the scan={}/{}\tsamples={:?}",
            escape_terminal_text(name),
            tally.bodies,
            tally.with_string,
            escape_terminal_text(&property),
            tally.agreed,
            tally.comparable,
            tally.samples
        );
        let mut scanned = tally.scanned_properties.iter().collect::<Vec<_>>();
        scanned.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
        for (property, count) in scanned.iter().take(3) {
            println!(
                "\t\tthe scan read {} instead in {count}",
                escape_terminal_text(property)
            );
        }
    }
    Ok(())
}

/// Where a class's records keep the first string their declarations read.
#[derive(Default)]
pub(crate) struct DeclaredNameTally {
    pub(crate) bodies: usize,
    pub(crate) with_string: usize,
    /// The declaring `class.property`, counted so a class with more than one
    /// answer shows it rather than hiding behind the first record.
    pub(crate) properties: BTreeMap<String, usize>,
    /// Records where the scan also returned a name.
    pub(crate) comparable: usize,
    /// Of those, records where the two agree.
    pub(crate) agreed: usize,
    /// Of the rest, the declaration whose value the scan returned instead.
    pub(crate) scanned_properties: BTreeMap<String, usize>,
    pub(crate) samples: Vec<String>,
}

/// Walk every record of one class against the schema and report how far the
/// declared properties explain the body. A body is only "exact" when the
/// declarations tile it with nothing left over.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // One pass plus its report.
pub(crate) fn serial_probe(
    path: &Path,
    class_name: &str,
    stream: bool,
    record_mode: bool,
    rows: usize,
    dump_remaining: Option<usize>,
    dump_stop: Option<&str>,
    trace_id: Option<u32>,
    dump_body: Option<u32>,
    element: Option<u32>,
    dump_window: usize,
    dump_count: usize,
    faces_only: bool,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let class_index = schema_class_index(Some(&schema), class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("schema class not found: {class_name}"),
        )
    })?;
    let partition_paths = partition_paths(&container);

    let mut attempted = 0_usize;
    let mut exact = 0_usize;
    let mut trailing = 0_usize;
    let mut stopped = 0_usize;
    let mut stops: BTreeMap<String, usize> = BTreeMap::new();
    let mut trailing_bytes: BTreeMap<usize, usize> = BTreeMap::new();
    let mut references = 0_usize;
    let mut objects = 0_usize;
    let mut pending = 0_usize;
    let mut dumped = 0_usize;
    let mut trailer_matches = 0_usize;
    let mut boundary_records = 0_usize;
    let mut boundary_exact = 0_usize;
    let mut boundary_faces = 0_usize;
    let mut boundary_exact_faces = 0_usize;
    let mut drained = 0_usize;
    let mut drained_exact = 0_usize;

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                attempted += 1;
                let header_walk = rvt_model::walk_object(&schema, class_index, body);
                if dump_remaining == Some(header_walk.remaining)
                    && header_walk.stop.is_none()
                    && dumped < dump_count
                {
                    dumped += 1;
                    let mut named = String::new();
                    for reference in &header_walk.references {
                        if !named.is_empty() {
                            named.push(' ');
                        }
                        let class = schema
                            .class_by_index(reference.class_index)
                            .map_or("<unknown>", |class| class.name.as_str());
                        let _ = write!(named, "{}:{class}", reference.object_id);
                    }
                    let mut tail = String::new();
                    for byte in body.get(header_walk.consumed..).unwrap_or_default() {
                        let _ = write!(tail, "{byte:02x}");
                    }
                    println!(
                        "  id={} len={} header={} refs=[{named}]",
                        header.id,
                        body.len(),
                        header_walk.consumed
                    );
                    println!("    tail={tail}");
                }
                if dump_body == Some(header.id) {
                    let mut hexed = String::new();
                    for byte in body {
                        let _ = write!(hexed, "{byte:02x}");
                    }
                    println!("body id={} len={} {hexed}", header.id, body.len());
                }
                if trace_id == Some(header.id) && dumped == 0 {
                    dumped += 1;
                    let (result, trace) = rvt_model::walk_record_traced(&schema, class_index, body);
                    println!("Trace of record {} ({} bytes):", header.id, body.len());
                    for entry in &trace {
                        println!(
                            "  {:6} +{:<4} {}.{}",
                            entry.offset, entry.consumed, entry.class, entry.property
                        );
                    }
                    println!(
                        "  stop_at={} {}",
                        result.stop_offset,
                        result
                            .stop
                            .as_ref()
                            .map_or_else(|| "none".to_owned(), describe_serial_stop)
                    );
                }
                if element == Some(header.id) {
                    let (result, objects) =
                        rvt_model::walk_record_collecting(&schema, class_index, body);
                    report_boundary_topology(&schema, header.id, body.len(), &result, &objects);
                    report_declared_bodies(&schema, body, &objects);
                }
                if record_mode {
                    let result = rvt_model::walk_record(&schema, class_index, body);
                    let faces = result
                        .references
                        .iter()
                        .filter(|reference| {
                            schema
                                .class_by_index(reference.class_index)
                                .is_some_and(|class| class.name == "Face")
                        })
                        .count();
                    if faces > 0 {
                        boundary_records += 1;
                        boundary_faces += faces;
                        if result.is_exact() {
                            boundary_exact += 1;
                            boundary_exact_faces += faces;
                        }
                    }
                    // `--faces-only` narrows every tally below to the records
                    // geometry is actually read out of, which tile exactly far
                    // less often than the file average.
                    if faces_only && faces == 0 {
                        continue;
                    }
                    if result.pending_references == 0 {
                        drained += 1;
                        drained_exact += usize::from(result.is_exact());
                    }
                    objects += result.nodes;
                    pending += result.pending_references;
                    references += result.references.len();
                    trailer_matches += usize::from(result.length_trailer_matches);
                    if result.is_exact() {
                        exact += 1;
                    } else if let Some(stop) = &result.stop {
                        stopped += 1;
                        let described = describe_serial_stop(stop);
                        if dump_stop.is_some_and(|wanted| described.contains(wanted))
                            && dumped < dump_count
                        {
                            dumped += 1;
                            let from = result.stop_offset.saturating_sub(dump_window);
                            let to = (result.stop_offset + dump_window).min(body.len());
                            let mut before = String::new();
                            for byte in body.get(from..result.stop_offset).unwrap_or_default() {
                                let _ = write!(before, "{byte:02x}");
                            }
                            let mut after = String::new();
                            for byte in body.get(result.stop_offset..to).unwrap_or_default() {
                                let _ = write!(after, "{byte:02x}");
                            }
                            println!(
                                "  id={} len={} stop_at={} {described}",
                                header.id,
                                body.len(),
                                result.stop_offset
                            );
                            println!("    before={before} | after={after}");
                        }
                        *stops.entry(described).or_default() += 1;
                    } else {
                        trailing += 1;
                        if dump_remaining == Some(result.remaining) && dumped < dump_count {
                            dumped += 1;
                            let mut tail = String::new();
                            for byte in body
                                .get(result.consumed..body.len() - RECORD_LENGTH_TRAILER_BYTES)
                                .unwrap_or_default()
                            {
                                let _ = write!(tail, "{byte:02x}");
                            }
                            let (_, walked) =
                                rvt_model::walk_record_collecting(&schema, class_index, body);
                            let mut walked_classes = String::new();
                            for object in walked.iter().rev().take(6).rev() {
                                let class = schema
                                    .class_by_index(object.class_index)
                                    .map_or("<unknown>", |class| class.name.as_str());
                                let _ = write!(walked_classes, " {class}/{}", object.bytes);
                            }
                            println!(
                                "  id={} len={} consumed={} pending={} objects={} tail={tail}",
                                header.id,
                                body.len(),
                                result.consumed,
                                result.pending_references,
                                walked.len()
                            );
                            println!("    last:{walked_classes}");
                        }
                        *trailing_bytes.entry(result.remaining).or_default() += 1;
                    }
                    continue;
                }
                let (remaining, stop, found, read_objects) = if stream {
                    let result = rvt_model::walk_object_stream(&schema, class_index, body);
                    objects += result.objects;
                    pending += result.pending_references;
                    (
                        result.remaining,
                        result.stop,
                        result.references.len(),
                        result.objects,
                    )
                } else {
                    let result = rvt_model::walk_object(&schema, class_index, body);
                    (result.remaining, result.stop, result.references.len(), 0)
                };
                let _ = read_objects;
                references += found;
                match &stop {
                    None if remaining == 0 => exact += 1,
                    None => {
                        trailing += 1;
                        *trailing_bytes.entry(remaining).or_default() += 1;
                    }
                    Some(stop) => {
                        stopped += 1;
                        *stops.entry(describe_serial_stop(stop)).or_default() += 1;
                    }
                }
            }
        },
    )?;

    if faces_only {
        // The tallies below cover only face-bearing records; report their
        // count, not the file's, so the percentages have the right base.
        attempted = boundary_records;
    }

    let share = |part: usize| {
        if attempted == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / attempted as f64
            }
        }
    };
    println!("Class: {class_name} [{class_index}]");
    println!("Records walked: {attempted}");
    println!("  explained exactly: {exact} ({:.1}%)", share(exact));
    println!(
        "  declarations read, bytes left over: {trailing} ({:.1}%)",
        share(trailing)
    );
    println!("  stopped early: {stopped} ({:.1}%)", share(stopped));
    println!("  node references read: {references}");
    if stream || record_mode {
        println!("  referenced objects walked: {objects}");
        println!("  references never reached: {pending}");
    }
    if record_mode {
        println!(
            "  records carrying boundary faces: {boundary_records} ({boundary_exact} explained exactly)"
        );
        println!("  faces in them: {boundary_faces} ({boundary_exact_faces} in explained records)");
        // A record whose reference queue drains before its body does has read
        // every object the body holds; one with references left over stopped
        // because the body ended, and its last references name objects stored
        // elsewhere. The two end differently, so tally them apart.
        println!(
            "  reference queue drained before the body: {drained} ({drained_exact} explained exactly)"
        );
        println!(
            "  trailing length word matches the body length: {trailer_matches} ({:.1}%)",
            share(trailer_matches)
        );
    }
    if !stops.is_empty() {
        println!("Where the walk stopped:");
        let mut ranked = stops.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        for (reason, count) in ranked.iter().take(rows) {
            println!("  {count:8}  {reason}");
        }
    }
    if !trailing_bytes.is_empty() {
        println!("Bytes left over:");
        let mut ranked = trailing_bytes.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        for (remaining, count) in ranked.iter().take(rows) {
            println!("  {count:8} records  {remaining} bytes");
        }
    }
    Ok(())
}

/// Check the width the walk reads `GInfo.m_flags` at against the width the
/// bytes that follow prove, over every node the corpus holds.
///
/// The check assumes no rule: for a node whose first declaration after the
/// inherited `GNode.m_GInfo` is a reference the schema fixes the class of, the
/// width is read off the following bytes, and the walk's own reading is scored
/// against it. This is what refuted the reading that took the lead word's top
/// bit as a width marker - it labelled 73 354 sites four bytes and not one
/// site two - and it is what would catch a regression the record tallies
/// cannot see, since a record can tile with a compensating pair of errors.
///
/// `--node-class` narrows the report to one class and adds the raw words from
/// `m_flags` on. That is how the `EdgeLoop` question was settled: under the old
/// reading its `m_nextLoop` read as the nonsense `id=8 class=0` in all 67 691
/// loops of an exactly-explained record, harmless only because class 0 does not
/// resolve, where the declared width gives a null identifier followed directly
/// by `m_pFace`.
#[allow(clippy::too_many_lines)] // One pass over the corpus plus its tables.
pub(crate) fn flags_probe(
    path: &Path,
    class_name: &str,
    rows: usize,
    faces_only: bool,
    every_record: bool,
    node_class: Option<&str>,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let class_index = schema_class_index(Some(&schema), class_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("schema class not found: {class_name}"),
        )
    })?;
    let partition_paths = partition_paths(&container);

    let mut records = 0_usize;
    let mut exact = 0_usize;
    let mut sampled_records = 0_usize;
    let mut samples_seen = 0_usize;
    let mut samples: Vec<rvt_model::FlagWidthSample> = Vec::new();

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (result, found) =
                    rvt_model::walk_record_flag_widths(&schema, class_index, body);
                if faces_only && !carries_faces(&schema, &result) {
                    continue;
                }
                records += 1;
                exact += usize::from(result.is_exact());
                samples_seen += found.len();
                // A site inside a record that did not tile may sit at an
                // offset the walk got wrong, and then its label is a label of
                // the wrong bytes. Exactly-explained records are the ones
                // whose offsets are known good.
                if every_record || result.is_exact() {
                    sampled_records += 1;
                    samples.extend(found);
                }
            }
        },
    )?;

    let share = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / whole as f64
            }
        }
    };
    println!("Class: {class_name} [{class_index}]");
    println!("Records walked: {records}");
    println!(
        "  explained exactly: {exact} ({:.1}%)",
        share(exact, records)
    );
    println!("Node GInfo objects met: {samples_seen}");
    println!(
        "  kept from {sampled_records} records{}: {}",
        if every_record {
            ""
        } else {
            " explained exactly"
        },
        samples.len()
    );

    let mut dataset: Vec<(rvt_model::FlagWidthSample, usize)> = Vec::new();
    let mut checkable = 0_usize;
    let mut ambiguous = 0_usize;
    for sample in &samples {
        if sample.checkable {
            checkable += 1;
        }
        match sample.proved {
            Some(proved) => dataset.push((*sample, proved)),
            None if sample.checkable => ambiguous += 1,
            None => {}
        }
    }
    let proved_short = dataset.iter().filter(|(_, width)| *width == 2).count();
    let proved_long = dataset.iter().filter(|(_, width)| *width == 4).count();
    let disagreed = dataset
        .iter()
        .filter(|(sample, width)| sample.read != *width)
        .count();
    println!("Of them checkable by the reference oracle: {checkable}");
    println!("  the bytes prove four: {proved_long}");
    println!("  the bytes prove two: {proved_short}");
    println!("  unlabelled, neither width or both land on a legal reference: {ambiguous}");
    println!(
        "  where the walk read a different width: {disagreed} ({:.2}%)",
        share(disagreed, dataset.len())
    );

    if let Some(wanted) = node_class {
        dataset.retain(|(sample, _)| {
            schema
                .class_by_index(sample.node_class)
                .is_some_and(|class| class.name == wanted)
        });
        println!(
            "Restricted to {wanted} objects: {} labelled sites",
            dataset.len()
        );
    }

    let split = |key: &dyn Fn(&rvt_model::FlagWidthSample) -> String| {
        let mut table: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (sample, width) in &dataset {
            let entry = table.entry(key(sample)).or_default();
            if *width == 4 {
                entry.1 += 1;
            } else {
                entry.0 += 1;
            }
        }
        table
    };
    let report = |title: &str, table: &BTreeMap<String, (usize, usize)>| {
        let mixed: usize = table
            .values()
            .map(|(short, long)| short.min(long))
            .sum::<usize>();
        println!(
            "{title}: {} values, {mixed} of them holding both widths",
            table.len()
        );
        let mut ranked = table.iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            (right.1.0 + right.1.1)
                .cmp(&(left.1.0 + left.1.1))
                .then(left.0.cmp(right.0))
        });
        for (value, (short, long)) in ranked.iter().take(rows) {
            println!("  {value:>30}  two={short:<8} four={long}");
        }
    };

    report(
        "By the node's class",
        &split(&|sample| {
            schema.class_by_index(sample.node_class).map_or_else(
                || format!("{}", sample.node_class),
                |class| class.name.clone(),
            )
        }),
    );
    if node_class.is_some() {
        // The raw words, so the two readings can be compared byte for byte:
        // under two bytes the object's next declaration starts at word 1,
        // under the declared four at word 2.
        report(
            "By the words from m_flags on",
            &split(&|sample| {
                let mut shown = String::new();
                for word in sample.words {
                    let _ = write!(shown, "{word:04x} ");
                }
                shown.trim_end().to_owned()
            }),
        );
        let reference_at = |sample: &rvt_model::FlagWidthSample, word: usize| {
            let class = sample.words[word + 2];
            format!(
                "id={} {}",
                u32::from(sample.words[word]) | (u32::from(sample.words[word + 1]) << 16),
                schema
                    .class_by_index(class)
                    .map_or_else(|| format!("class {class}"), |class| class.name.clone())
            )
        };
        report(
            "By the reference a two-byte read lands on",
            &split(&|sample| reference_at(sample, 1)),
        );
        report(
            "By the reference the declared width lands on",
            &split(&|sample| reference_at(sample, 2)),
        );
    }
    Ok(())
}

/// Whether a walked record named any boundary face.
pub(crate) fn carries_faces(schema: &Schema, walk: &rvt_model::SerialRecordWalk) -> bool {
    walk.references.iter().any(|reference| {
        schema
            .class_by_index(reference.class_index)
            .is_some_and(|class| class.name == "Face")
    })
}

/// What bodies one record declares, and how each stands against the box the
/// same record carries.
///
/// This is the per-record form of the question [`place_declared_body`] answers
/// in bulk: a record is a set of `GBRep` nodes, and the reason a body appears
/// to disagree with its box is nearly always that two of them were read as
/// one.
pub(crate) fn report_declared_bodies(
    schema: &Schema,
    record_body: &[u8],
    objects: &[rvt_model::SerialObject],
) {
    let Some(classes) = brep_class_indexes(Some(schema)) else {
        return;
    };
    let assembled =
        rvt_model::assemble_symbol_brep(objects, &classes, &brep_body_classes(Some(schema)));
    if assembled.bodies.is_empty() {
        return;
    }
    let bounds = schema_class_index(Some(schema), "GNode").and_then(|gnode| {
        GElementGraphFields::parse(record_body, |class_index| {
            schema_class_is_a(schema, class_index, gnode)
        })
    });
    println!(
        "    bodies declared by GBRep nodes: {} (box {})",
        assembled.bodies.len(),
        bounds.as_ref().map_or_else(
            || "not read".to_owned(),
            |graph| format!("{:?} {:?}", graph.bounds.min, graph.bounds.max)
        )
    );
    for (index, declared) in assembled.bodies.iter().enumerate() {
        let sub = assembled.body(index);
        let extent = sub.as_ref().and_then(body_extent_feet);
        let residual = sub
            .as_ref()
            .zip(bounds.as_ref())
            .and_then(|(sub, graph)| body_bounds_residual_feet(sub, &graph.bounds));
        println!(
            "      node {} faces={} edges={} one-sided={} open={} solid={} closed={} extent={extent:?} residual={residual:?}",
            declared.node_id,
            declared.faces.len(),
            declared.edges,
            declared.one_sided_edges,
            declared.open_edges,
            declared.is_solid(),
            declared.is_closed(),
        );
        let Some(sub) = sub else { continue };
        for (face, face_id) in sub.faces.iter().zip(&sub.face_ids) {
            let Some(marks) = objects
                .iter()
                .find(|object| object.object_id == *face_id)
                .and_then(GFaceMarks::read)
            else {
                continue;
            };
            let extent = faces_extent_feet(std::slice::from_ref(face));
            let outside = extent.zip(bounds.as_ref()).map(|(extent, graph)| {
                face_reaches_outside(extent, &graph.bounds, BODY_BOUNDS_TOLERANCE_FEET)
            });
            println!("        face {face_id} {marks} outside={outside:?} extent={extent:?}");
        }
    }
}

/// One candidate mark a face might carry, and what it is read from.
///
/// Each is a field the schema declares on `GFace` or on the `GInfo` every
/// `GNode` opens with - nothing here is searched for or inferred from the
/// geometry. What the probe asks of each is the same question: does it fire on
/// exactly the faces that reach outside the box the record itself carries?
pub(crate) struct FaceMark {
    pub(crate) name: &'static str,
    pub(crate) fires: fn(&GFaceMarks) -> bool,
}

pub(crate) const FACE_MARKS: &[FaceMark] = &[
    FaceMark {
        name: "m_cutType != 0",
        fires: |marks| marks.cut_type != 0,
    },
    FaceMark {
        name: "m_renderStyleId is null",
        fires: |marks| marks.render_style_id < 0,
    },
    FaceMark {
        name: "GInfo.m_flags & 0x80000 clear",
        fires: |marks| marks.info_flags & FACE_INSIDE_THE_BOX_FLAG == 0,
    },
    FaceMark {
        name: "m_faceFlags_v9 & 0x2 set",
        fires: |marks| marks.face_flags & 0x2 != 0,
    },
];

/// What one mark scored, over every face the file declares and every record
/// whose bodies miss its box.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FaceMarkScore {
    /// Faces the mark fires on that reach outside their record's box.
    pub(crate) marked_outside: u64,
    /// Faces it fires on that lie inside it.
    pub(crate) marked_inside: u64,
    /// Faces outside the box that it does not fire on.
    pub(crate) unmarked_outside: u64,
    /// Records whose bodies miss the box and where dropping the faces this
    /// mark fires on leaves exactly one body reproducing it. This is what the
    /// mark is for, and the number to read.
    pub(crate) records_recovered: u64,
    /// ...of which the recovered body's boundary closes, and of which it is a
    /// solid. A body that only closes once its own geometry is thrown away is
    /// not a wall.
    pub(crate) records_recovered_closed: u64,
    pub(crate) records_recovered_solid: u64,
    /// ...and of which the kept faces bound a volume by their own loops. See
    /// [`rvt_model::SymbolBrep::bounds_a_volume`]: this is the closure an exporter needs, and
    /// the topological one cannot see it because the trimmed faces are still
    /// named by the edges that reach them.
    pub(crate) records_recovered_bounded: u64,
    /// Records where the trim leaves more than one body on the box, so the
    /// answer is ambiguous and nothing can be selected.
    pub(crate) records_ambiguous: u64,
    /// The control, measured on records the box already resolves: how many
    /// would stop being resolved if the same trim were applied there too.
    /// A trim is only ever a second pass, so this costs nothing - it says how
    /// much real geometry the mark would take if it were a first one.
    pub(crate) placed_records_broken: u64,
}

/// Score every declared face mark against the record's own box.
///
/// A `GElement` record's box bounds what the element draws. A body that
/// reaches outside it holds faces that are not the element's surface - on a
/// wall, the geometry of the openings cut into it, which the file joins into
/// the same shell. This asks each field the format declares on a face whether
/// it separates those two sets, and reports the one measurement that can
/// accept such a rule: how many records whose bodies miss the box land exactly
/// one body on it once the marked faces are dropped.
///
/// The trim is measured the way it would be applied - the marked `Face`
/// objects are removed and the record is assembled again - so the body counts
/// its own edges afterwards and can say whether what is left still closes.
#[allow(clippy::too_many_lines)] // One streaming pass plus its report.
pub(crate) fn face_mark_probe(path: &Path, max_member_bytes: u64) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?;
    let classes = brep_class_indexes(schema.as_ref());
    let body_classes = brep_body_classes(schema.as_ref());
    let geometry_element_class_index = schema_class_index(schema.as_ref(), "GElement");
    let gnode_class_index = schema_class_index(schema.as_ref(), "GNode");
    let (Some(schema), Some(classes), Some(geometry_element_class_index), Some(gnode_class_index)) = (
        schema.as_ref(),
        classes,
        geometry_element_class_index,
        gnode_class_index,
    ) else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "the schema does not declare the boundary-representation classes",
        )));
    };
    let partition_paths = partition_paths(&container);

    let mut records = 0_u64;
    let mut records_placed = 0_u64;
    let mut records_ambiguous = 0_u64;
    let mut bodies = 0_u64;
    let mut faces_read = 0_u64;
    let mut faces_declared = 0_u64;
    let mut faces_outside = 0_u64;
    let mut scores = vec![FaceMarkScore::default(); FACE_MARKS.len()];
    // Which bodies of an assembly reproduce the box, and whether that is
    // exactly one - the same test `place_declared_body` applies.
    let placed_bodies = |assembled: &rvt_model::SymbolBrep, bounds: &GElementBounds| {
        (0..assembled.bodies.len())
            .filter(|index| {
                assembled
                    .body(*index)
                    .is_some_and(|body| body_is_placed_in(&body, bounds))
            })
            .collect::<Vec<_>>()
    };
    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let Some(graph) = GElementGraphFields::parse(body, |class_index| {
                    schema_class_is_a(schema, class_index, gnode_class_index)
                }) else {
                    continue;
                };
                if !graph.bounds.is_volumetric() {
                    continue;
                }
                let (_walk, objects) =
                    rvt_model::walk_record_collecting(schema, header.class_index, body);
                if !objects
                    .iter()
                    .any(|object| object.class_index == classes.face)
                {
                    continue;
                }
                let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &body_classes);
                if assembled.bodies.is_empty() {
                    continue;
                }
                records += 1;
                bodies += assembled.bodies.len() as u64;
                let placed = placed_bodies(&assembled, &graph.bounds);
                records_placed += u64::from(placed.len() == 1);
                records_ambiguous += u64::from(placed.len() > 1);

                // The face-level cross-tab, over the faces the record's bodies
                // hold: what each mark fires on, against whether the face
                // reaches outside the box.
                let marks: std::collections::HashMap<u32, GFaceMarks> = objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                    .filter_map(|object| {
                        GFaceMarks::read(object).map(|marks| (object.object_id, marks))
                    })
                    .collect();
                faces_declared += objects
                    .iter()
                    .filter(|object| object.class_index == classes.face)
                    .count() as u64;
                faces_read += assembled.faces.len() as u64;
                for (face, face_id) in assembled.faces.iter().zip(&assembled.face_ids) {
                    let outside =
                        faces_extent_feet(std::slice::from_ref(face)).is_some_and(|extent| {
                            face_reaches_outside(extent, &graph.bounds, BODY_BOUNDS_TOLERANCE_FEET)
                        });
                    faces_outside += u64::from(outside);
                    let Some(face_marks) = marks.get(face_id) else {
                        continue;
                    };
                    for (mark, score) in FACE_MARKS.iter().zip(&mut scores) {
                        match ((mark.fires)(face_marks), outside) {
                            (true, true) => score.marked_outside += 1,
                            (true, false) => score.marked_inside += 1,
                            (false, true) => score.unmarked_outside += 1,
                            (false, false) => {}
                        }
                    }
                }

                // The trim, assembled the way it would be applied.
                for (mark, score) in FACE_MARKS.iter().zip(&mut scores) {
                    let trimmed_objects = objects
                        .iter()
                        .filter(|object| {
                            object.class_index != classes.face
                                || !marks.get(&object.object_id).is_some_and(mark.fires)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if trimmed_objects.len() == objects.len() {
                        continue;
                    }
                    let trimmed =
                        rvt_model::assemble_symbol_brep(&trimmed_objects, &classes, &body_classes);
                    let trimmed_placed = placed_bodies(&trimmed, &graph.bounds);
                    if placed.len() == 1 {
                        score.placed_records_broken += u64::from(trimmed_placed.len() != 1);
                        continue;
                    }
                    match trimmed_placed[..] {
                        [index] => {
                            score.records_recovered += 1;
                            if let Some(body) = trimmed.bodies.get(index) {
                                score.records_recovered_closed += u64::from(body.is_closed());
                                score.records_recovered_solid += u64::from(body.is_solid());
                            }
                            if let Some(body) = trimmed.body(index) {
                                score.records_recovered_bounded +=
                                    u64::from(body.bounds_a_volume());
                            }
                        }
                        [_, _, ..] => score.records_ambiguous += 1,
                        [] => {}
                    }
                }
            }
        },
    )?;

    let share = |part: u64, whole: u64| {
        if whole == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / whole as f64
            }
        }
    };
    println!("File: {}", path.display());
    println!("GElement records with a volumetric box and a declared body: {records}");
    println!("  bodies declared: {bodies}");
    println!(
        "  records where exactly one body reproduces the box: {records_placed} ({:.1}%)",
        share(records_placed, records)
    );
    println!("  records where more than one does: {records_ambiguous}");
    println!("  faces declared: {faces_declared}, of them resolved and in a body: {faces_read}");
    println!(
        "  faces reaching outside their record's box: {faces_outside} ({:.1}%)",
        share(faces_outside, faces_read)
    );
    println!("Each mark against that answer:");
    for (mark, score) in FACE_MARKS.iter().zip(&scores) {
        println!("  {}", mark.name);
        println!(
            "    fires on {} faces outside the box and {} inside it; {} outside faces it misses",
            score.marked_outside, score.marked_inside, score.unmarked_outside
        );
        println!(
            "    records recovered: {} (bounding a volume {}, two-sided {}, closed {}), left ambiguous: {}",
            score.records_recovered,
            score.records_recovered_bounded,
            score.records_recovered_solid,
            score.records_recovered_closed,
            score.records_ambiguous
        );
        println!(
            "    records the box already resolves that the same trim would break: {}",
            score.placed_records_broken
        );
    }
    Ok(())
}

/// Whether a face's own extent reaches outside the record's box by more than
/// `tolerance`. A face that lies inside it is part of what the box bounds; one
/// that reaches past it cannot be.
pub(crate) fn face_reaches_outside(
    extent: ([f64; 3], [f64; 3]),
    bounds: &GElementBounds,
    tolerance: f64,
) -> bool {
    let (min, max) = extent;
    (0..3).any(|axis| {
        min[axis] < bounds.min[axis] - tolerance || max[axis] > bounds.max[axis] + tolerance
    })
}

/// Summarize one record's node stream and check that its boundary topology
/// closes: every edge naming two faces that exist, every loop naming a face,
/// every face naming a loop. Nothing is inferred - only what was read.
#[allow(clippy::too_many_lines)] // One pass over the objects plus its report.
pub(crate) fn report_boundary_topology(
    schema: &Schema,
    id: u32,
    body_bytes: usize,
    walk: &rvt_model::SerialRecordWalk,
    objects: &[rvt_model::SerialObject],
) {
    let name_of = |class_index: u16| {
        schema
            .class_by_index(class_index)
            .map_or("<unknown>", |class| class.name.as_str())
    };
    let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
    for object in objects {
        *classes.entry(name_of(object.class_index)).or_default() += 1;
    }
    let mut ranked = classes.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    let mut summary = String::new();
    for (name, count) in ranked.iter().take(7) {
        if !summary.is_empty() {
            summary.push(' ');
        }
        let _ = write!(summary, "{name}x{count}");
    }
    println!(
        "  record {id}: {body_bytes} bytes, exact={}, objects={}",
        walk.is_exact(),
        objects.len()
    );
    println!("    classes: {summary}");

    for object in objects.iter().filter(|object| {
        matches!(
            name_of(object.class_index),
            "RuledSurf"
                | "GLine"
                | "GArc"
                | "GEllipse"
                | "Face"
                | "Edge"
                | "EdgeLoop"
                | "EdgeLoopWithChainEnvelopes"
                | "InstanceInfo"
                | "GInstance"
                | "Geometry"
        )
    }) {
        println!(
            "    object {} {} refs={:?} ids={:?} ints={:?} alts={:?} numbers={:?}",
            object.object_id,
            name_of(object.class_index),
            object.references,
            object.identifiers,
            object.integers,
            object.alternate_integers,
            object.numbers
        );
    }

    let ids_of = |wanted: &str| {
        objects
            .iter()
            .filter(|object| name_of(object.class_index) == wanted)
            .map(|object| object.object_id)
            .collect::<BTreeSet<_>>()
    };
    let faces = ids_of("Face");
    let loops = ids_of("EdgeLoop");
    if faces.is_empty() {
        return;
    }

    let mut edges = 0_usize;
    let mut edges_with_two_faces = 0_usize;
    let mut edges_resolved = 0_usize;
    let mut loops_resolved = 0_usize;
    let mut faces_with_loop = 0_usize;
    let mut faces_with_surface = 0_usize;
    let mut surfaces: BTreeMap<&str, usize> = BTreeMap::new();
    for object in objects {
        match name_of(object.class_index) {
            "Edge" => {
                edges += 1;
                let named = object.identifiers.iter().take(2).collect::<BTreeSet<_>>();
                if named.len() == 2 {
                    edges_with_two_faces += 1;
                }
                if named.iter().all(|id| faces.contains(id)) {
                    edges_resolved += 1;
                }
            }
            "EdgeLoop" => {
                if object
                    .identifiers
                    .first()
                    .is_some_and(|id| faces.contains(id))
                {
                    loops_resolved += 1;
                }
            }
            "Face" => {
                if object
                    .references
                    .first()
                    .is_some_and(|reference| loops.contains(&reference.object_id))
                {
                    faces_with_loop += 1;
                }
                // `Face.m_pSurf` is the last reference the class declares.
                if let Some(surface) = object
                    .references
                    .last()
                    .filter(|reference| reference.object_id != 0)
                {
                    faces_with_surface += 1;
                    *surfaces.entry(name_of(surface.class_index)).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    println!(
        "    faces {} ({faces_with_loop} name a loop that exists, {faces_with_surface} name a surface)",
        faces.len()
    );
    println!(
        "    loops {} ({loops_resolved} name a face that exists)",
        loops.len()
    );
    println!(
        "    edges {edges} ({edges_with_two_faces} name two distinct faces, {edges_resolved} of those exist)"
    );
    // Surfaces carry their frame as plain numbers: an `Envelope` of four,
    // then the axes. Checking the frame is orthonormal and the radius positive
    // tests the decode itself, not just the topology.
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value).unwrap_or(f64::NAN);
    let unit = |v: &[f64]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let dot = |a: &[f64], b: &[f64]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let mut planes_checked = 0_usize;
    let mut planes_orthonormal = 0_usize;
    let mut cylinders_checked = 0_usize;
    let mut cylinders_orthonormal = 0_usize;
    let mut radii: Vec<f64> = Vec::new();
    for object in objects {
        let numbers = &object.numbers;
        match name_of(object.class_index) {
            // Envelope(4), origin(3), xVec(3), yVec(3)
            "Plane" if numbers.len() >= 13 => {
                planes_checked += 1;
                let (x, y) = (&numbers[7..10], &numbers[10..13]);
                if (unit(x) - 1.0).abs() < 1.0e-9
                    && (unit(y) - 1.0).abs() < 1.0e-9
                    && dot(x, y).abs() < 1.0e-9
                {
                    planes_orthonormal += 1;
                }
            }
            // Envelope(4), center(3), xVec(3), yVec(3), zVec(3), radius(1)
            "CylSurf" if numbers.len() >= 17 => {
                cylinders_checked += 1;
                let (x, y, z) = (&numbers[7..10], &numbers[10..13], &numbers[13..16]);
                if (unit(x) - 1.0).abs() < 1.0e-9
                    && (unit(y) - 1.0).abs() < 1.0e-9
                    && (unit(z) - 1.0).abs() < 1.0e-9
                    && dot(x, y).abs() < 1.0e-9
                    && dot(x, z).abs() < 1.0e-9
                {
                    cylinders_orthonormal += 1;
                }
                radii.push(metres(numbers[16]) * 1000.0);
            }
            _ => {}
        }
    }
    if planes_checked > 0 || cylinders_checked > 0 {
        println!("    planes with an orthonormal frame: {planes_orthonormal} of {planes_checked}");
        println!(
            "    cylinders with an orthonormal frame: {cylinders_orthonormal} of {cylinders_checked}"
        );
        radii.sort_by(f64::total_cmp);
        radii.dedup_by(|left, right| (*left - *right).abs() < 1.0e-6);
        let mut listed = String::new();
        for radius in radii.iter().take(12) {
            if !listed.is_empty() {
                listed.push(' ');
            }
            let _ = write!(listed, "{radius:.1}");
        }
        println!("    distinct cylinder radii, mm: {listed}");
    }
    let mut surface_summary = String::new();
    for (name, count) in surfaces {
        if !surface_summary.is_empty() {
            surface_summary.push(' ');
        }
        let _ = write!(surface_summary, "{name}x{count}");
    }
    if !surface_summary.is_empty() {
        println!("    surfaces: {surface_summary}");
    }
}

pub(crate) fn describe_serial_stop(stop: &rvt_model::SerialStop) -> String {
    match stop {
        rvt_model::SerialStop::Truncated { class, property } => {
            format!("truncated at {class}.{property}")
        }
        rvt_model::SerialStop::Unsupported {
            class,
            property,
            reason,
        } => format!("unsupported {reason} at {class}.{property}"),
        rvt_model::SerialStop::UnknownClass { class_index } => {
            format!("unknown class [{class_index}]")
        }
        rvt_model::SerialStop::TooDeep => "class chain too deep".to_owned(),
    }
}

/// Census every `RuledSurf` the corpus writes.
///
/// After the parent `Surface`'s four-number envelope the class declares two
/// object references - `m_pProfileCurve1` and `m_pProfileCurve2` - and two
/// three-number points, `m_Point1` and `m_Point2`. Which of the four a given
/// surface actually uses is not stated anywhere, so this counts it rather
/// than assuming it: the class pair the references name, whether the named
/// object is written in the same record, what the points hold on each side,
/// and whether a face reaches its `RuledSurf` by identifier or by encounter
/// order the way `CylSurf` does.
#[allow(clippy::too_many_lines)] // One streaming pass and the tallies it fills.
pub(crate) fn ruled_surf_probe(
    path: &Path,
    rows: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let ruled_class_index = schema_class_index(Some(&schema), "RuledSurf")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no RuledSurf"))?;
    let face_class_index = schema_class_index(Some(&schema), "Face")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no Face"))?;
    let edge_class_index = schema_class_index(Some(&schema), "Edge")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no Edge"))?;
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no GElement"))?;
    let partition_paths = partition_paths(&container);

    let mut records_with_a_ruled_surface = 0_u64;
    let mut surfaces = 0_u64;
    let mut faces_naming_one = 0_u64;
    let mut numbers_widths: BTreeMap<usize, u64> = BTreeMap::new();
    let mut reference_counts: BTreeMap<usize, u64> = BTreeMap::new();
    let mut profile_pairs: BTreeMap<(String, String), u64> = BTreeMap::new();
    // Per side: is the reference live, does the named object exist in this
    // record, and is the matching point non-zero? A degenerate profile is
    // expected to show as a null reference beside a used point, but that is
    // the hypothesis, not the reading.
    let mut side_shape: BTreeMap<(usize, bool, bool, bool), u64> = BTreeMap::new();
    // Distinct identifiers among a record's `RuledSurf` objects, against how
    // many it holds. A sentinel-identified class collapses to one.
    let mut identifier_spread: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    let mut sentinel_identifiers: BTreeMap<u32, u64> = BTreeMap::new();
    // Does the count of faces naming a `RuledSurf` match the count of objects?
    // Encounter-order pairing is only sound where it does.
    let mut face_to_object_balance: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    let mut profile_end_params: BTreeMap<String, (u64, f64, f64)> = BTreeMap::new();
    // What the file's own `EdgePnt`s say about the surface's two axes. The
    // ruling axis of a ruled surface runs between the two profiles, so it
    // should span exactly [0, 1]; the other should span the profile's own
    // `m_endParams`. Measuring both ranges decides which axis is which
    // without assuming either.
    let mut u_low = f64::MAX;
    let mut u_high = f64::MIN;
    let mut v_low = f64::MAX;
    let mut v_high = f64::MIN;
    let mut edge_points_on_a_ruled_face = 0_u64;
    // Record identifiers to hand to `serial-probe --element` for a trace.
    let mut example_records: Vec<u32> = Vec::new();
    // `Surface.m_Envelope` is the surface's own parameter box. If a ruled
    // surface always states [0, 0, 1, 1] then both of its axes are normalised
    // and neither carries a profile's raw parameter.
    let mut envelopes: BTreeMap<String, u64> = BTreeMap::new();
    let mut v_outside_unit = 0_u64;
    // How often each axis holds still across an edge: a ruling is constant in
    // the along-profile axis, a profile-following edge in the ruling axis.
    let mut u_constant = 0_u64;
    let mut v_constant = 0_u64;
    let mut neither_constant = 0_u64;
    // The ruling axis should take its extreme values on the profiles
    // themselves, so tally how often an edge sits exactly at each end.
    let mut ruling_axis_endpoints: BTreeMap<String, u64> = BTreeMap::new();

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                let (_, objects) =
                    rvt_model::walk_record_collecting(&schema, header.class_index, body);
                let ruled: Vec<&rvt_model::SerialObject> = objects
                    .iter()
                    .filter(|object| object.class_index == ruled_class_index)
                    .collect();
                if ruled.is_empty() {
                    continue;
                }
                records_with_a_ruled_surface += 1;
                if example_records.len() < 8 {
                    example_records.push(header.id);
                }
                let present: BTreeSet<(u16, u32)> = objects
                    .iter()
                    .map(|object| (object.class_index, object.object_id))
                    .collect();
                let naming_faces = objects
                    .iter()
                    .filter(|object| object.class_index == face_class_index)
                    .filter(|object| {
                        object
                            .references
                            .last()
                            .is_some_and(|reference| reference.class_index == ruled_class_index)
                    })
                    .count();
                faces_naming_one += naming_faces as u64;
                *face_to_object_balance
                    .entry((naming_faces, ruled.len()))
                    .or_default() += 1;
                let distinct: BTreeSet<u32> = ruled.iter().map(|object| object.object_id).collect();
                *identifier_spread
                    .entry((distinct.len(), ruled.len()))
                    .or_default() += 1;
                if distinct.len() == 1 {
                    if let Some(identifier) = distinct.iter().next() {
                        *sentinel_identifiers.entry(*identifier).or_default() += 1;
                    }
                }
                // Pair faces to objects by encounter order, the way every
                // sentinel-identified surface class is paired, then read the
                // `EdgePnt`s the edges of those faces store on that side.
                let ruled_faces: BTreeSet<u32> = objects
                    .iter()
                    .filter(|object| object.class_index == face_class_index)
                    .filter(|object| {
                        object
                            .references
                            .last()
                            .is_some_and(|reference| reference.class_index == ruled_class_index)
                    })
                    .map(|object| object.object_id)
                    .collect();
                for edge in objects
                    .iter()
                    .filter(|object| object.class_index == edge_class_index)
                {
                    if edge.identifiers.len() != 6 || edge.numbers.len() < 8 {
                        continue;
                    }
                    let tail = &edge.numbers[edge.numbers.len() - 8..];
                    for side in 0..2 {
                        if !ruled_faces.contains(&edge.identifiers[side]) {
                            continue;
                        }
                        let (first_u, first_v) = (tail[side * 2], tail[side * 2 + 1]);
                        let (last_u, last_v) = (tail[4 + side * 2], tail[4 + side * 2 + 1]);
                        for value in [first_u, last_u] {
                            u_low = u_low.min(value);
                            u_high = u_high.max(value);
                        }
                        for value in [first_v, last_v] {
                            v_low = v_low.min(value);
                            v_high = v_high.max(value);
                        }
                        edge_points_on_a_ruled_face += 1;
                        for value in [first_v, last_v] {
                            if value < -1.0e-9 || value > 1.0 + 1.0e-9 {
                                v_outside_unit += 1;
                            }
                        }
                        let still_u = (first_u - last_u).abs() <= 1.0e-9;
                        let still_v = (first_v - last_v).abs() <= 1.0e-9;
                        match (still_u, still_v) {
                            (true, false) => u_constant += 1,
                            (false, true) => v_constant += 1,
                            _ => neither_constant += 1,
                        }
                        if still_u {
                            let at = if first_u.abs() <= 1.0e-9 {
                                "u held at 0"
                            } else if (first_u - 1.0).abs() <= 1.0e-9 {
                                "u held at 1"
                            } else {
                                "u held elsewhere"
                            };
                            *ruling_axis_endpoints.entry(at.to_owned()).or_default() += 1;
                        }
                        if still_v {
                            let at = if first_v.abs() <= 1.0e-9 {
                                "v held at 0"
                            } else if (first_v - 1.0).abs() <= 1.0e-9 {
                                "v held at 1"
                            } else {
                                "v held elsewhere"
                            };
                            *ruling_axis_endpoints.entry(at.to_owned()).or_default() += 1;
                        }
                    }
                }
                for object in &ruled {
                    surfaces += 1;
                    *numbers_widths.entry(object.numbers.len()).or_default() += 1;
                    if let Some(envelope) = object.numbers.get(0..4) {
                        *envelopes
                            .entry(format!(
                                "[{:.3}, {:.3}, {:.3}, {:.3}]",
                                envelope[0], envelope[1], envelope[2], envelope[3]
                            ))
                            .or_default() += 1;
                    }
                    *reference_counts.entry(object.references.len()).or_default() += 1;
                    let named = |side: usize| -> Option<&rvt_model::GElementNodeReference> {
                        object
                            .references
                            .get(side)
                            .filter(|reference| reference.object_id != 0)
                    };
                    let class_name = |side: usize| -> String {
                        named(side).map_or_else(
                            || "null".to_owned(),
                            |reference| {
                                schema.class_by_index(reference.class_index).map_or_else(
                                    || format!("class {}", reference.class_index),
                                    |class| class.name.clone(),
                                )
                            },
                        )
                    };
                    *profile_pairs
                        .entry((class_name(0), class_name(1)))
                        .or_default() += 1;
                    for side in 0..2 {
                        // `m_Point1` is numbers 4..7 and `m_Point2` 7..10, the
                        // envelope taking the first four.
                        let start = 4 + side * 3;
                        let point = object.numbers.get(start..start + 3);
                        let point_used =
                            point.is_some_and(|point| point.iter().any(|value| value.abs() > 0.0));
                        let reference = named(side);
                        let resolvable = reference.is_some_and(|reference| {
                            present.contains(&(reference.class_index, reference.object_id))
                        });
                        *side_shape
                            .entry((side, reference.is_some(), resolvable, point_used))
                            .or_default() += 1;
                        // A live profile's own parameter range says what `v`
                        // the ruling runs over; `GCurve.m_endParams` is the
                        // first two numbers of every curve.
                        if let Some(reference) = reference {
                            if let Some(curve) = objects.iter().find(|candidate| {
                                candidate.class_index == reference.class_index
                                    && candidate.object_id == reference.object_id
                            }) {
                                if let Some(params) = curve.numbers.get(0..2) {
                                    let entry = profile_end_params
                                        .entry(class_name(side))
                                        .or_insert((0, f64::MAX, f64::MIN));
                                    entry.0 += 1;
                                    entry.1 = entry.1.min(params[0]);
                                    entry.2 = entry.2.max(params[1]);
                                }
                            }
                        }
                    }
                }
            }
        },
    )?;

    println!("RuledSurf census for {}", path.display());
    println!("Records holding at least one: {records_with_a_ruled_surface}");
    println!("RuledSurf objects: {surfaces}");
    println!("Faces naming one as their surface: {faces_naming_one}");

    println!("Float64 values per object:");
    for (width, count) in numbers_widths.iter().take(rows) {
        println!("  {width}\t{count}");
    }
    println!("References per object:");
    for (count, records) in reference_counts.iter().take(rows) {
        println!("  {count}\t{records}");
    }

    println!("Profile pair (m_pProfileCurve1, m_pProfileCurve2):");
    let mut ranked: Vec<_> = profile_pairs.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((first, second), count) in ranked.iter().take(rows) {
        println!("  {count}\t{first} -> {second}");
    }

    println!("Per side (side, reference live, names an object in this record, point non-zero):");
    let mut sides: Vec<_> = side_shape.into_iter().collect();
    sides.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((side, live, resolvable, point_used), count) in sides.iter().take(rows) {
        println!(
            "  {count}\tm_Point{} live={live} resolvable={resolvable} point={point_used}",
            side + 1
        );
    }

    println!("Profile parameter range by class (from GCurve.m_endParams):");
    for (name, (count, low, high)) in profile_end_params.iter().take(rows) {
        println!("  {name}\tn={count} min={low:.6} max={high:.6}");
    }

    println!("Surface.m_Envelope values:");
    let mut boxes: Vec<_> = envelopes.into_iter().collect();
    boxes.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (envelope, count) in boxes.iter().take(rows) {
        println!("  {count}\t{envelope}");
    }
    println!("Edge v values outside [0, 1]: {v_outside_unit}");
    println!("Example records (serial-probe --element): {example_records:?}");
    println!("EdgePnt values on ruled faces: {edge_points_on_a_ruled_face} edge sides");
    if edge_points_on_a_ruled_face > 0 {
        println!("  u spans {u_low:.6} .. {u_high:.6}");
        println!("  v spans {v_low:.6} .. {v_high:.6}");
    }
    println!("  u held constant across the edge: {u_constant}");
    println!("  v held constant across the edge: {v_constant}");
    println!("  neither held constant: {neither_constant}");
    for (label, count) in &ruling_axis_endpoints {
        println!("  {count}\t{label}");
    }
    println!("Distinct identifiers vs objects per record:");
    let mut spread: Vec<_> = identifier_spread.into_iter().collect();
    spread.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((distinct, held), count) in spread.iter().take(rows) {
        println!("  {count}\t{distinct} distinct of {held}");
    }
    println!("Shared identifier when a record collapses to one:");
    let mut sentinels: Vec<_> = sentinel_identifiers.into_iter().collect();
    sentinels.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (identifier, count) in sentinels.iter().take(rows) {
        println!("  {count}\t0x{identifier:08x}");
    }
    println!("Faces naming one vs objects held, per record:");
    let mut balance: Vec<_> = face_to_object_balance.into_iter().collect();
    balance.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for ((naming, held), count) in balance.iter().take(rows) {
        println!("  {count}\t{naming} faces, {held} objects");
    }
    Ok(())
}

/// Walk every `GElement` record, decode its inherited `GGroup.m_subNodes`
/// reference array, and report which node classes appear and whether their
/// object identifiers resolve against the record identifiers in the file.
/// Nothing here is exported; the probe only measures what is reachable.
#[allow(clippy::too_many_lines)] // One streaming pass keeps large RVT payloads out of memory.
pub(crate) fn geometry_graph_probe(
    path: &Path,
    rows: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let schema = read_schema(&container)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the class schema is required for this probe",
        )
    })?;
    let geometry_element_class_index = schema_class_index(Some(&schema), "GElement")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "this schema has no GElement"))?;
    let gnode_class_index = schema_class_index(Some(&schema), "GNode");
    let partition_paths = partition_paths(&container);

    // Every distinct identifier/class pair and the member each identifier was
    // written in, so a node reference can be tested against both.
    let mut record_classes: BTreeSet<(u32, u16)> = BTreeSet::new();
    let mut member_records: BTreeSet<(u64, u32)> = BTreeSet::new();
    let mut graphs: Vec<(u64, Vec<rvt_model::GElementNodeReference>)> = Vec::new();
    let mut geometry_element_records = 0_usize;
    let mut strict_graphs = 0_usize;
    let mut member_key = 0_u64;
    // Bodies are large. Ranking every class by the bytes it occupies says
    // where the payload actually is, independent of any structural guess.
    let mut class_bytes: BTreeMap<u16, (usize, u64)> = BTreeMap::new();
    let mut node_identifier_maximum = 0_u32;
    // Node references are (u32 object id, u16 class index). Scanning whole
    // GElement bodies for that shape reaches nodes nested below the top level.
    // A random six bytes match only if the class index is one of the few dozen
    // GNode subclasses and the identifier stays tiny, so the scan is quiet.
    let mut nested_classes: BTreeMap<u16, usize> = BTreeMap::new();
    let mut gnode_subclasses: BTreeSet<u16> = BTreeSet::new();
    if let Some(gnode_class_index) = gnode_class_index {
        for class in &schema.classes {
            if schema_class_is_a(&schema, class.index, gnode_class_index) {
                gnode_subclasses.insert(class.index);
            }
        }
    }

    for_each_member(
        &container,
        &partition_paths,
        max_member_bytes,
        |_, _, _, layout, walk, payload| {
            member_key += 1;
            for record in &walk.records {
                let Some(header) = RecordHeader::parse(payload, record, layout) else {
                    continue;
                };
                record_classes.insert((header.id, header.class_index));
                member_records.insert((member_key, header.id));
                let entry = class_bytes.entry(header.class_index).or_default();
                entry.0 += 1;
                entry.1 += u64::from(header.body_bytes);
                if header.class_index != geometry_element_class_index {
                    continue;
                }
                geometry_element_records += 1;
                let body = payload
                    .get(record.body_offset()..record.end())
                    .unwrap_or_default();
                if let Some(gnode_class_index) = gnode_class_index {
                    strict_graphs += usize::from(
                        GElementGraphFields::parse(body, |class_index| {
                            schema_class_is_a(&schema, class_index, gnode_class_index)
                        })
                        .is_some(),
                    );
                }
                for window in body.windows(6) {
                    let object_id =
                        u32::from_le_bytes([window[0], window[1], window[2], window[3]]);
                    let class_index = u16::from_le_bytes([window[4], window[5]]);
                    if object_id > 0
                        && object_id <= NESTED_NODE_IDENTIFIER_LIMIT
                        && gnode_subclasses.contains(&class_index)
                    {
                        *nested_classes.entry(class_index).or_default() += 1;
                    }
                }
                // The permissive rule accepts any class the schema knows, so
                // the histogram is not narrowed by the GNode assumption.
                if let Some(graph) = GElementGraphFields::parse(body, |class_index| {
                    schema.class_by_index(class_index).is_some()
                }) {
                    graphs.push((member_key, graph.top_level_nodes));
                }
            }
        },
    )?;

    let mut node_classes: BTreeMap<u16, usize> = BTreeMap::new();
    let mut node_counts: BTreeMap<usize, usize> = BTreeMap::new();
    let mut references = 0_usize;
    let mut class_agreed = 0_usize;
    let mut same_member = 0_usize;
    for (member_key, nodes) in &graphs {
        *node_counts.entry(nodes.len()).or_default() += 1;
        for node in nodes {
            references += 1;
            node_identifier_maximum = node_identifier_maximum.max(node.object_id);
            *node_classes.entry(node.class_index).or_default() += 1;
            class_agreed +=
                usize::from(record_classes.contains(&(node.object_id, node.class_index)));
            same_member += usize::from(member_records.contains(&(*member_key, node.object_id)));
        }
    }

    let share = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 * 100.0 / whole as f64
            }
        }
    };
    println!("GElement records: {geometry_element_records}");
    println!(
        "  with a decodable node array and bounds: {} ({:.1}%)",
        graphs.len(),
        share(graphs.len(), geometry_element_records)
    );
    println!("  accepted by the strict GNode rule: {strict_graphs}");
    println!("Top-level node references: {references}");
    println!("Nodes per GElement:");
    for (count, occurrences) in node_counts.iter().take(rows) {
        println!("  {count:6} nodes  {occurrences} GElements");
    }
    println!("Node classes:");
    let mut ranked = node_classes.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for (class_index, count) in ranked.iter().take(rows) {
        let name = schema
            .class_by_index(*class_index)
            .map_or("<unknown>", |class| class.name.as_str());
        println!(
            "  {count:8} ({:5.1}%)  {name} [{class_index}]",
            share(*count, references)
        );
    }
    println!("Record classes by total body bytes:");
    let mut by_bytes = class_bytes.into_iter().collect::<Vec<_>>();
    by_bytes.sort_by(|left, right| right.1.1.cmp(&left.1.1).then(left.0.cmp(&right.0)));
    let total_bytes = by_bytes.iter().map(|(_, (_, bytes))| *bytes).sum::<u64>();
    for (class_index, (count, bytes)) in by_bytes.iter().take(rows) {
        let name = schema
            .class_by_index(*class_index)
            .map_or("<unknown>", |class| class.name.as_str());
        #[allow(clippy::cast_precision_loss)]
        let percent = if total_bytes == 0 {
            0.0
        } else {
            *bytes as f64 * 100.0 / total_bytes as f64
        };
        println!(
            "  {bytes:12} bytes ({percent:5.1}%) in {count:8} records  {name} [{class_index}]"
        );
    }
    println!("Node-shaped references anywhere in GElement bodies:");
    let mut nested = nested_classes.into_iter().collect::<Vec<_>>();
    nested.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    let nested_total = nested.iter().map(|(_, count)| *count).sum::<usize>();
    println!("  total: {nested_total}");
    for (class_index, count) in nested.iter().take(rows) {
        let name = schema
            .class_by_index(*class_index)
            .map_or("<unknown>", |class| class.name.as_str());
        println!(
            "  {count:8} ({:5.1}%)  {name} [{class_index}]",
            share(*count, nested_total)
        );
    }
    // Whether these identifiers are element identifiers is decided by their
    // range and by whether they land on a record in the member that holds the
    // GElement, not by a global lookup: every small integer exists as some
    // record identifier, so a global hit rate says nothing.
    println!("Node identifier space:");
    println!("  largest top-level node object identifier: {node_identifier_maximum}");
    println!(
        "  also a record identifier in the same member: {same_member} ({:.1}%)",
        share(same_member, references)
    );
    println!(
        "  that record carries the declared class: {class_agreed} ({:.1}%)",
        share(class_agreed, references)
    );
    Ok(())
}

impl IfcSettingsArguments<'_> {
    /// The setup this run uses: the file where one is given, then every flag
    /// that was actually passed, in that order.
    pub(crate) fn resolve(&self) -> Result<ExportSettings, Box<dyn Error>> {
        let mut settings = match self.settings {
            Some(path) => ExportSettings::from_json_file(path)?,
            None => ExportSettings::default(),
        };
        if let Some(unit) = self.length_unit {
            settings.length_unit = unit.into();
        }
        if self.no_revit_property_sets {
            settings.property_sets.revit_parameters = false;
        }
        if self.no_revit_type_property_sets {
            settings.property_sets.revit_type_parameters = false;
        }
        if self.no_ifc_common_property_sets {
            settings.property_sets.ifc_common = false;
        }
        if self.no_base_quantities {
            settings.property_sets.base_quantities = false;
        }
        if self.no_shared_bodies {
            settings.shared_bodies = false;
        }
        if self.elements_without_a_body {
            settings.elements_without_a_body = true;
        }
        if self.no_types {
            settings.types = false;
        }
        if self.no_openings {
            settings.openings = false;
        }
        if let Some(path) = self.class_mapping {
            settings.class_mapping_file = Some(path.to_path_buf());
        }
        // The table is read here rather than by the writer: a setup that names
        // a file it cannot read fails before a model is decoded, not after.
        settings.load_class_mapping()?;
        Ok(settings)
    }
}

/// Report which classes own the decoded bodies, and which classes of model
/// element reach one. `rivet brep` says how much geometry comes out of the
/// file; this says whose it is and what carries it out.
pub(crate) fn body_owners(
    path: &Path,
    classes: usize,
    max_member_bytes: u64,
) -> Result<(), Box<dyn Error>> {
    let recovered = recover_elements(path, max_member_bytes)?;
    let schema = recovered.schema.as_ref();
    let rows = tally_class_geometry(&recovered.elements, |element| {
        element.class_index.and_then(|index| {
            schema
                .and_then(|schema| schema.class_by_index(index))
                .map(|class| class.name.as_str())
        })
    });

    let total = rows
        .values()
        .fold(ClassGeometry::default(), |mut total, row| {
            total.body_ids += row.body_ids;
            total.body_records += row.body_records;
            total.complete_bodies += row.complete_bodies;
            total.faces += row.faces;
            total.named_by_an_instance += row.named_by_an_instance;
            total.verified_by_an_instance += row.verified_by_an_instance;
            total.bodies_with_bounds += row.bodies_with_bounds;
            total.bodies_matching_their_bounds += row.bodies_matching_their_bounds;
            total.bodies_with_graph_bounds += row.bodies_with_graph_bounds;
            total.bodies_matching_their_graph_bounds += row.bodies_matching_their_graph_bounds;
            total.bodies_placed_only_by_graph_bounds += row.bodies_placed_only_by_graph_bounds;
            total.bodies_placed_only_by_near_duplicate_bounds +=
                row.bodies_placed_only_by_near_duplicate_bounds;
            total.bodies_placed_only_by_another_box += row.bodies_placed_only_by_another_box;
            total.graph_bounds_differing_from_exact += row.graph_bounds_differing_from_exact;
            total.bodies_away_from_the_origin += row.bodies_away_from_the_origin;
            total.model_elements += row.model_elements;
            total.model_elements_with_their_own_body += row.model_elements_with_their_own_body;
            total.model_elements_with_a_placed_body += row.model_elements_with_a_placed_body;
            total.model_elements_with_a_verified_symbol_body +=
                row.model_elements_with_a_verified_symbol_body;
            total
        });
    println!(
        "Element ids owning a decoded body: {} from {} body-bearing GElement records",
        total.body_ids, total.body_records
    );
    println!(
        "  bodies held out by keeping one per id: {}",
        total.body_records.saturating_sub(total.body_ids)
    );
    println!(
        "  complete bodies: {} ({} faces)",
        total.complete_bodies, total.faces
    );
    println!(
        "  ids an instance names as its symbol: {} ({} bounds-verified)",
        total.named_by_an_instance, total.verified_by_an_instance
    );
    println!(
        "  bodies reproducing their record's own bounds: {} of {} that carry one",
        total.bodies_matching_their_bounds, total.bodies_with_bounds
    );
    println!(
        "  bodies whose centre is over a foot from the origin: {}",
        total.bodies_away_from_the_origin
    );
    println!(
        "  bodies reproducing their record's graph-header box: {} of {} that carry one \
         ({} of those boxes are not the exact block)",
        total.bodies_matching_their_graph_bounds,
        total.bodies_with_graph_bounds,
        total.graph_bounds_differing_from_exact
    );
    println!(
        "  of the bodies with no exact block, placed by the graph box: {}, by the near \
         duplicate: {}, by either: {}",
        total.bodies_placed_only_by_graph_bounds,
        total.bodies_placed_only_by_near_duplicate_bounds,
        total.bodies_placed_only_by_another_box
    );

    print_body_box_residuals(&recovered.elements);
    print_near_miss_anatomy(
        &recovered.elements,
        |element| {
            element.class_index.and_then(|index| {
                schema
                    .and_then(|schema| schema.class_by_index(index))
                    .map(|class| class.name.as_str())
            })
        },
        classes,
    );

    print_body_owner_table(&rows, classes);
    print_model_element_body_table(&rows, &total, classes);
    // The same funnel the export prints, here too: this is the command for
    // reading what the decode is worth, and the nested-family path is decoded
    // geometry that does or does not reach an element.
    report_nested_assembly_funnel(&geometry_statistics(
        &recovered.elements,
        recovered.schema.as_ref(),
    ));
    Ok(())
}

/// How closely each box on a body's own record reproduces that body.
///
/// A second placement tier is only worth adding if agreement with the graph
/// header's box is as sharp as agreement with the exact block: a box that is
/// merely near the body is a different box, not a looser reading of the same
/// one. The rows split on whether the record also carried an exact block,
/// because the bodies that carry none are the ones a second tier would add.
pub(crate) fn print_body_box_residuals(elements: &BTreeMap<u32, ExportedElement>) {
    /// Upper bound of each bucket in Revit internal feet, and its label.
    const BUCKETS: [(f64, &str); 6] = [
        (BODY_BOUNDS_TOLERANCE_FEET, "<=1e-6ft"),
        (1e-4, "<=1e-4ft"),
        (1e-2, "<=1e-2ft"),
        (1.0, "<=1ft"),
        (100.0, "<=100ft"),
        (f64::INFINITY, ">100ft"),
    ];
    // Rows: graph box with an exact block present, graph box without one,
    // near duplicate without one. Columns: the buckets, then "no such box".
    let mut rows = [[0_usize; BUCKETS.len() + 1]; 3];
    for element in elements.values() {
        if element.brep.is_none() {
            continue;
        }
        let residuals = element.brep_box_residuals;
        let measured: &[(usize, Option<f64>)] = if residuals.exact.is_some() {
            &[(0, residuals.graph)]
        } else {
            &[(1, residuals.graph), (2, residuals.near_duplicate)]
        };
        for &(row, residual) in measured {
            let column = residual.map_or(BUCKETS.len(), |residual| {
                BUCKETS
                    .iter()
                    .position(|(bound, _)| residual <= *bound)
                    .unwrap_or(BUCKETS.len() - 1)
            });
            rows[row][column] += 1;
        }
    }
    println!("\nHow far each box sits from the body on its own record:");
    print!("box");
    for (_, label) in BUCKETS {
        print!("\t{label}");
    }
    println!("\tno box");
    for (row, name) in [
        ("graph box, exact block present", 0),
        ("graph box, no exact block", 1),
        ("near duplicate, no exact block", 2),
    ]
    .map(|(name, row)| (row, name))
    {
        print!("{name}");
        for count in rows[row] {
            print!("\t{count}");
        }
        println!();
    }
}

/// What a body that misses its own record's box actually differs by.
///
/// A residual alone cannot say why: the same tenth of a foot is a body sitting
/// somewhere else, a body that is the wrong size, or a body missing a face.
/// Each is a different repair, so each is counted separately. The three
/// readings are independent of one another and of the residual buckets above.
pub(crate) fn print_near_miss_anatomy<'a>(
    elements: &BTreeMap<u32, ExportedElement>,
    class_name: impl Fn(&ExportedElement) -> Option<&'a str>,
    classes: usize,
) {
    #[derive(Default)]
    struct Anatomy {
        misses: usize,
        /// The body has the box's extent on every axis and sits elsewhere:
        /// a transform the decode does not apply.
        same_size_shifted: usize,
        /// The body lies inside the box on every axis: geometry not decoded,
        /// or a box drawn around more than this body.
        inside_the_box: usize,
        /// ...and of those, how many are bodies with an excluded face, which
        /// is the reading that would explain it.
        inside_and_incomplete: usize,
        /// The body reaches outside its own box on some axis.
        outside_the_box: usize,
        /// Bodies whose id carries more than one body-bearing record, so the
        /// box may belong to a body this one displaced.
        from_a_multi_record_id: usize,
        /// What the record declares - see [`place_declared_body`]: how many
        /// bodies, how many of those reproduce the box, and how many are
        /// solids rather than free surfaces.
        declared_bodies: usize,
        bodies_matching_the_box: usize,
        solid_bodies: usize,
        /// Records with no declared body at all: nothing to select from.
        with_no_declared_body: usize,
    }
    let mut rows: BTreeMap<&str, Anatomy> = BTreeMap::new();
    let mut total = Anatomy::default();
    for element in elements.values() {
        let (Some(brep), Some(bounds)) = (&element.brep, element.brep_placement_box) else {
            continue;
        };
        let Some(residual) = body_bounds_residual_feet(brep, &bounds) else {
            continue;
        };
        if residual <= BODY_BOUNDS_TOLERANCE_FEET {
            continue;
        }
        let Some((min, max)) = body_extent_feet(brep) else {
            continue;
        };
        let size_residual = (0..3)
            .map(|axis| ((max[axis] - min[axis]) - (bounds.max[axis] - bounds.min[axis])).abs())
            .fold(0.0_f64, f64::max);
        let inside = (0..3).all(|axis| {
            min[axis] >= bounds.min[axis] - BODY_BOUNDS_TOLERANCE_FEET
                && max[axis] <= bounds.max[axis] + BODY_BOUNDS_TOLERANCE_FEET
        });
        let matching = (0..brep.bodies.len())
            .filter(|index| {
                brep.body(*index)
                    .is_some_and(|body| body_is_placed_in(&body, &bounds))
            })
            .count();
        let solids = brep.bodies.iter().filter(|body| body.is_solid()).count();
        let row = rows
            .entry(class_name(element).unwrap_or(UNRESOLVED_CLASS))
            .or_default();
        for row in [row, &mut total] {
            row.misses += 1;
            row.same_size_shifted += usize::from(size_residual <= BODY_BOUNDS_TOLERANCE_FEET);
            row.inside_the_box += usize::from(inside);
            row.inside_and_incomplete += usize::from(inside && !brep.excluded_faces.is_empty());
            row.outside_the_box += usize::from(!inside);
            row.from_a_multi_record_id += usize::from(element.brep_records > 1);
            row.declared_bodies += brep.bodies.len();
            row.bodies_matching_the_box += matching;
            row.solid_bodies += solids;
            row.with_no_declared_body += usize::from(brep.bodies.is_empty());
        }
    }
    let mut by_misses = rows.iter().collect::<Vec<_>>();
    by_misses.sort_by(|left, right| {
        right
            .1
            .misses
            .cmp(&left.1.misses)
            .then_with(|| left.0.cmp(right.0))
    });
    println!("\nWhat a body missing its own box differs by:");
    println!(
        "class\tmisses\tsame size, moved\tinside it\toutside it\tmulti-record id\t\
         declared bodies\t=box\tsolid\tno body"
    );
    let listed = by_misses
        .into_iter()
        .take(classes)
        .map(|(name, row)| (*name, row))
        .chain(std::iter::once(("total", &total)));
    for (name, row) in listed {
        println!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.misses,
            row.same_size_shifted,
            row.inside_the_box,
            row.outside_the_box,
            row.from_a_multi_record_id,
            row.declared_bodies,
            row.bodies_matching_the_box,
            row.solid_bodies,
            row.with_no_declared_body
        );
    }
}

/// Which classes own the decoded bodies, most bodies first.
pub(crate) fn print_body_owner_table(rows: &BTreeMap<&str, ClassGeometry>, classes: usize) {
    let mut by_bodies = rows.iter().collect::<Vec<_>>();
    by_bodies.sort_by(|left, right| {
        right
            .1
            .body_ids
            .cmp(&left.1.body_ids)
            .then_with(|| left.0.cmp(right.0))
    });
    println!("\nWho owns the bodies:");
    println!(
        "class\tids\trecords\tcomplete\tfaces\tnamed\tverified\t=bounds\t+graph\t+dup\t+either\toff-origin"
    );
    for (name, row) in by_bodies
        .iter()
        .filter(|(_, row)| row.body_ids > 0)
        .take(classes)
    {
        println!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}/{}\t{}\t{}\t{}\t{}",
            row.body_ids,
            row.body_records,
            row.complete_bodies,
            row.faces,
            row.named_by_an_instance,
            row.verified_by_an_instance,
            row.bodies_matching_their_bounds,
            row.bodies_with_bounds,
            row.bodies_placed_only_by_graph_bounds,
            row.bodies_placed_only_by_near_duplicate_bounds,
            row.bodies_placed_only_by_another_box,
            row.bodies_away_from_the_origin
        );
    }
}

/// Which classes of model element reach a body, most elements first.
pub(crate) fn print_model_element_body_table(
    rows: &BTreeMap<&str, ClassGeometry>,
    total: &ClassGeometry,
    classes: usize,
) {
    let mut by_model_elements = rows.iter().collect::<Vec<_>>();
    by_model_elements.sort_by(|left, right| {
        right
            .1
            .model_elements
            .cmp(&left.1.model_elements)
            .then_with(|| left.0.cmp(right.0))
    });
    println!("\nWhich model elements reach a body:");
    println!("class\tmodel elements\town body\tplaced\tverified symbol body");
    for (name, row) in by_model_elements
        .iter()
        .filter(|(_, row)| row.model_elements > 0)
        .take(classes)
    {
        println!(
            "{name}\t{}\t{}\t{}\t{}",
            row.model_elements,
            row.model_elements_with_their_own_body,
            row.model_elements_with_a_placed_body,
            row.model_elements_with_a_verified_symbol_body
        );
    }
    println!(
        "total\t{}\t{}\t{}\t{}",
        total.model_elements,
        total.model_elements_with_their_own_body,
        total.model_elements_with_a_placed_body,
        total.model_elements_with_a_verified_symbol_body
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reading `rebuild_rings` encodes: `Edge.identifiers` is
    /// `[pFace0, pFace1, next0, next1, ...]`, so the ring around a face
    /// continues at `next[side]` and ends where the chain leaves the edges.
    #[test]
    fn walks_the_edges_of_a_face_into_one_ring() {
        // A triangle on face 1, its far side on faces 2, 3 and 4, ending on
        // loop 99. Edge 12 also carries a link for face 2 that goes nowhere,
        // which is the shape of every ring no loop of a record ends.
        let links = BTreeMap::from([
            (10_u32, [1_u32, 2, 11, 98]),
            (11, [1, 3, 12, 97]),
            (12, [1, 4, 99, 96]),
        ]);
        let edges = [(10_u32, 0_usize), (11, 0), (12, 0)];
        assert_eq!(
            rebuild_rings(1, &edges, &links),
            Ok(vec![(10, 99, 3)]),
            "one ring of three edges, ending on the loop"
        );
        // Face 2 is named by one edge whose link names no edge of this record:
        // a fragment, not a boundary.
        assert_eq!(rebuild_rings(2, &[(10, 1)], &links), Ok(vec![(10, 98, 1)]));
        assert_eq!(
            rebuild_rings(5, &[], &links),
            Err("no edge names the face".to_owned())
        );
    }
}
