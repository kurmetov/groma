use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Read, Seek, SeekFrom},
};

use flate2::read::DeflateDecoder;

use crate::{REVIT_PAGE_CHECKSUM_BYTES, REVIT_PAGE_PAYLOAD_BYTES, REVIT_STORED_PAGE_BYTES};

const GZIP_MAGIC: [u8; 3] = [0x1f, 0x8b, 0x08];
const GZIP_FIXED_HEADER_LEN: usize = 10;
const GZIP_FLAG_HEADER_CRC: u8 = 0x02;
const GZIP_FLAG_EXTRA: u8 = 0x04;
const GZIP_FLAG_NAME: u8 = 0x08;
const GZIP_FLAG_COMMENT: u8 = 0x10;
const GZIP_FLAG_RESERVED: u8 = 0xe0;
const MAX_GZIP_HEADER_BYTES: usize = 1024 * 1024;
pub const PARTITION_MEMBER_PREFIX_BYTES: usize = 16;
/// Fixed descriptor written immediately before every compressed member.
pub const MEMBER_DESCRIPTOR_BYTES: usize = 40;
/// Bytes the descriptor's stored span counts on top of the gzip member.
pub const MEMBER_STORED_SPAN_OVERHEAD: u64 = 16;
/// Width of the `[class index:u16][zero:u16]` marker an envelope is built around.
pub const MARKER_ENVELOPE_MARKER_BYTES: usize = 4;
/// Upper bound on retained context bytes on either side of one marker.
pub const MAX_MARKER_ENVELOPE_CONTEXT_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionReadOptions {
    pub max_candidates: usize,
    pub max_member_decoded_bytes: u64,
    pub max_total_decoded_bytes: u64,
    /// Inclusive schema-index range for experimental `[u16 tag][u16 zero]`
    /// counting. `None` disables the scan.
    pub class_prefix_range: Option<(u16, u16)>,
    /// Bounded byte context retained around one schema marker. `None`
    /// disables envelope capture.
    pub marker_envelope: Option<MarkerEnvelopeOptions>,
}

impl Default for PartitionReadOptions {
    fn default() -> Self {
        Self {
            max_candidates: 1_000_000,
            max_member_decoded_bytes: 256 * 1024 * 1024,
            max_total_decoded_bytes: 8 * 1024 * 1024 * 1024,
            class_prefix_range: None,
            marker_envelope: None,
        }
    }
}

/// The 40-byte record written immediately before a compressed member.
///
/// Only fields with a corpus-wide invariant are named; the rest keep neutral
/// names and the raw bytes stay available through [`MemberDescriptor::bytes`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemberDescriptor {
    /// `+0`. Varies per member; its derivation is unknown.
    pub checksum_word: u32,
    /// `+4`. Decoded size of the preceding member in this partition.
    pub previous_decoded_bytes: u32,
    /// `+8`. `0x0E47` except in the first descriptor of a partition.
    pub previous_tag: u16,
    /// `+10`. Stored span of the preceding member.
    pub previous_stored_span: u32,
    /// `+14`. Observed as `0x0E4E` throughout the corpus.
    pub tag: u16,
    /// `+16`. Small value, observed as 4-7.
    pub flags: u16,
    /// `+18`. Observed as zero.
    pub reserved: u16,
    /// `+20`. Candidate record count for this member.
    pub record_count: u32,
    /// `+24`. Gzip bytes of this member plus [`MEMBER_STORED_SPAN_OVERHEAD`].
    pub stored_span: u32,
    /// `+28`. Candidate total of record body bytes in the decoded member.
    pub record_body_bytes: u32,
    /// `+32`. Observed as 101, 102, or 103; selects the record header width.
    pub format_tag: u32,
    /// `+36`. Observed as zero.
    pub trailing_word: u32,
    raw: [u8; MEMBER_DESCRIPTOR_BYTES],
}

impl MemberDescriptor {
    fn parse(raw: [u8; MEMBER_DESCRIPTOR_BYTES]) -> Self {
        let u16_at = |offset: usize| u16::from_le_bytes([raw[offset], raw[offset + 1]]);
        let u32_at = |offset: usize| {
            u32::from_le_bytes([
                raw[offset],
                raw[offset + 1],
                raw[offset + 2],
                raw[offset + 3],
            ])
        };
        Self {
            checksum_word: u32_at(0),
            previous_decoded_bytes: u32_at(4),
            previous_tag: u16_at(8),
            previous_stored_span: u32_at(10),
            tag: u16_at(14),
            flags: u16_at(16),
            reserved: u16_at(18),
            record_count: u32_at(20),
            stored_span: u32_at(24),
            record_body_bytes: u32_at(28),
            format_tag: u32_at(32),
            trailing_word: u32_at(36),
            raw,
        }
    }

    /// The descriptor exactly as stored.
    #[must_use]
    pub fn bytes(&self) -> &[u8; MEMBER_DESCRIPTOR_BYTES] {
        &self.raw
    }

    /// Whether the declared stored span matches the member actually inflated.
    #[must_use]
    pub fn matches_stored_span(&self, compressed_bytes: u64) -> bool {
        u64::from(self.stored_span) == compressed_bytes + MEMBER_STORED_SPAN_OVERHEAD
    }
}

/// Capture bounds for byte context around one schema-marker candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarkerEnvelopeOptions {
    /// Schema class index that must appear as `[index:u16][zero:u16]`.
    pub class_index: u16,
    /// Bytes retained before the marker; fewer are kept near a member start.
    pub leading_bytes: usize,
    /// Bytes retained after the marker; fewer are kept near a member end.
    pub trailing_bytes: usize,
    /// Envelopes retained per partition stream. Later candidates are counted
    /// but not stored, and the partition is reported as truncated.
    pub max_envelopes: usize,
}

/// Byte context around one marker candidate, with its decoded coordinates.
///
/// The offset is relative to the start of the member's inflated byte stream,
/// so it stays stable regardless of how the member was chunked while reading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkerEnvelope {
    pub member_index: usize,
    /// Offset of the marker's first byte inside the decoded member.
    pub decoded_offset: u64,
    bytes: Vec<u8>,
    marker_position: usize,
}

impl MarkerEnvelope {
    /// Leading context, marker, and trailing context as one contiguous slice.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes preceding the marker; shorter than requested near a member start.
    #[must_use]
    pub fn leading(&self) -> &[u8] {
        &self.bytes[..self.marker_position]
    }

    #[must_use]
    pub fn marker(&self) -> &[u8] {
        &self.bytes[self.marker_position..self.marker_position + MARKER_ENVELOPE_MARKER_BYTES]
    }

    /// Bytes following the marker; shorter than requested near a member end.
    #[must_use]
    pub fn trailing(&self) -> &[u8] {
        &self.bytes[self.marker_position + MARKER_ENVELOPE_MARKER_BYTES..]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionMember {
    pub index: usize,
    /// Offset after checksum-page trailers have been removed.
    pub logical_offset: u64,
    /// Offset in the byte-exact CFB stream.
    pub stored_offset: u64,
    /// Gzip header plus raw DEFLATE bytes, excluding page trailers.
    pub compressed_bytes: u64,
    pub decoded_bytes: u64,
    decoded_prefix: [u8; PARTITION_MEMBER_PREFIX_BYTES],
    decoded_prefix_len: u8,
    pub class_prefix_candidates: u64,
    /// Marker candidates seen in this member, including uncaptured ones.
    pub marker_candidates: u64,
    /// Descriptor preceding this member, when the stream holds a complete one.
    pub descriptor: Option<MemberDescriptor>,
}

impl PartitionMember {
    /// First up-to-16 decoded bytes, retained for bounded structural probes.
    #[must_use]
    pub fn decoded_prefix(&self) -> &[u8] {
        &self.decoded_prefix[..usize::from(self.decoded_prefix_len)]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionFailure {
    pub logical_offset: u64,
    pub stored_offset: u64,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionReport {
    pub path: String,
    pub stored_bytes: u64,
    pub logical_bytes: u64,
    pub full_checksum_pages: u64,
    pub gzip_candidates: usize,
    pub skipped_embedded_candidates: usize,
    pub members: Vec<PartitionMember>,
    pub failures: Vec<PartitionFailure>,
    pub total_decoded_bytes: u64,
    pub class_prefix_counts: BTreeMap<u16, u64>,
    pub marker_envelopes: Vec<MarkerEnvelope>,
    /// Marker candidates seen in this partition, including uncaptured ones.
    pub marker_candidates: u64,
    /// Set when the envelope budget stopped capture before the last candidate.
    pub marker_envelopes_truncated: bool,
    pub truncated_by_limit: bool,
}

pub(crate) fn analyze_partition<R: Read + Seek>(
    path: String,
    reader: &mut R,
    stored_bytes: u64,
    options: PartitionReadOptions,
) -> io::Result<PartitionReport> {
    let full_checksum_pages = stored_bytes / REVIT_STORED_PAGE_BYTES as u64;
    let logical_bytes = stored_bytes
        .checked_sub(full_checksum_pages * REVIT_PAGE_CHECKSUM_BYTES as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "page length underflow"))?;

    validate_marker_envelope_options(options.marker_envelope)?;
    let candidates = find_gzip_candidates(reader, stored_bytes, options.max_candidates)?;
    let mut members = Vec::new();
    let mut failures = Vec::new();
    let mut skipped_embedded_candidates = 0;
    let mut covered_until = 0;
    let mut total_decoded_bytes = 0_u64;
    let mut class_prefix_counts = BTreeMap::<u16, u64>::new();
    let mut marker_envelopes = Vec::new();
    let mut marker_candidates = 0_u64;
    let mut marker_envelopes_truncated = false;
    let mut truncated_by_limit = false;

    for logical_offset in candidates.iter().copied() {
        if logical_offset < covered_until {
            skipped_embedded_candidates += 1;
            continue;
        }

        let stored_offset = logical_to_stored_offset(logical_offset, stored_bytes);
        let mut cleaned = ChecksumPageReader::new(reader, stored_bytes, logical_offset)?;
        let member_options = member_decode_options(options, members.len(), marker_envelopes.len());
        match decode_member(&mut cleaned, member_options) {
            Ok(mut decoded) => {
                let Some(next_total) = total_decoded_bytes.checked_add(decoded.decoded_bytes)
                else {
                    failures.push(PartitionFailure {
                        logical_offset,
                        stored_offset,
                        message: "aggregate decoded-byte count overflowed".to_owned(),
                    });
                    truncated_by_limit = true;
                    break;
                };
                if next_total > options.max_total_decoded_bytes {
                    failures.push(PartitionFailure {
                        logical_offset,
                        stored_offset,
                        message: format!(
                            "aggregate decoded bytes would exceed {}",
                            options.max_total_decoded_bytes
                        ),
                    });
                    truncated_by_limit = true;
                    break;
                }

                let index = members.len();
                covered_until = logical_offset.saturating_add(decoded.compressed_bytes);
                total_decoded_bytes = next_total;
                for (tag, count) in &decoded.class_prefix_counts {
                    let aggregate = class_prefix_counts.entry(*tag).or_default();
                    *aggregate = aggregate.saturating_add(*count);
                }
                marker_candidates = marker_candidates.saturating_add(decoded.marker_candidates);
                marker_envelopes_truncated |= decoded.marker_envelopes_truncated;
                marker_envelopes.extend(std::mem::take(&mut decoded.marker_envelopes));
                let descriptor = read_member_descriptor(reader, stored_bytes, logical_offset);
                members.push(decoded.into_member(index, logical_offset, stored_offset, descriptor));
            }
            Err(MemberDecodeFailure::Invalid(message)) => failures.push(PartitionFailure {
                logical_offset,
                stored_offset,
                message,
            }),
            Err(MemberDecodeFailure::Limit(message)) => {
                failures.push(PartitionFailure {
                    logical_offset,
                    stored_offset,
                    message,
                });
                truncated_by_limit = true;
                break;
            }
        }
    }

    Ok(PartitionReport {
        path,
        stored_bytes,
        logical_bytes,
        full_checksum_pages,
        gzip_candidates: candidates.len(),
        skipped_embedded_candidates,
        members,
        failures,
        total_decoded_bytes,
        class_prefix_counts,
        marker_envelopes,
        marker_candidates,
        marker_envelopes_truncated,
        truncated_by_limit,
    })
}

/// Inflate one member into memory, starting at its checksum-clean offset.
///
/// The caller supplies the byte budget; nothing is retained beyond the
/// returned payload.
pub(crate) fn decode_member_bytes<R: Read + Seek>(
    reader: &mut R,
    stored_bytes: u64,
    logical_offset: u64,
    limit: u64,
) -> io::Result<Vec<u8>> {
    let mut cleaned = ChecksumPageReader::new(reader, stored_bytes, logical_offset)?;
    read_gzip_header(&mut cleaned)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let mut decoder = DeflateDecoder::new(&mut cleaned);
    let mut payload = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = decoder.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if payload.len() as u64 + count as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("decoded member exceeds {limit} bytes"),
            ));
        }
        payload.extend_from_slice(&buffer[..count]);
    }
    if decoder.total_in() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty DEFLATE body",
        ));
    }
    Ok(payload)
}

/// Read the fixed descriptor that precedes a member, when one fits before it.
///
/// A member at the very start of a stream has no descriptor, and an unreadable
/// descriptor is reported as absent rather than as a member-level failure.
fn read_member_descriptor<R: Read + Seek>(
    reader: &mut R,
    stored_bytes: u64,
    logical_offset: u64,
) -> Option<MemberDescriptor> {
    let start = logical_offset.checked_sub(MEMBER_DESCRIPTOR_BYTES as u64)?;
    let mut cleaned = ChecksumPageReader::new(reader, stored_bytes, start).ok()?;
    let mut raw = [0_u8; MEMBER_DESCRIPTOR_BYTES];
    cleaned.read_exact(&mut raw).ok()?;
    Some(MemberDescriptor::parse(raw))
}

fn validate_marker_envelope_options(options: Option<MarkerEnvelopeOptions>) -> io::Result<()> {
    let Some(envelope) = options else {
        return Ok(());
    };
    if envelope.leading_bytes > MAX_MARKER_ENVELOPE_CONTEXT_BYTES
        || envelope.trailing_bytes > MAX_MARKER_ENVELOPE_CONTEXT_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "marker envelope context exceeds {MAX_MARKER_ENVELOPE_CONTEXT_BYTES} bytes per side"
            ),
        ));
    }
    Ok(())
}

/// Per-member options, with the envelope budget reduced by what earlier
/// members in the same partition already captured.
fn member_decode_options(
    options: PartitionReadOptions,
    member_index: usize,
    captured_envelopes: usize,
) -> MemberDecodeOptions {
    MemberDecodeOptions {
        limit: options.max_member_decoded_bytes,
        class_prefix_range: options.class_prefix_range,
        marker_envelope: options.marker_envelope.map(|mut envelope| {
            envelope.max_envelopes = envelope.max_envelopes.saturating_sub(captured_envelopes);
            envelope
        }),
        member_index,
    }
}

fn find_gzip_candidates<R: Read + Seek>(
    reader: &mut R,
    stored_bytes: u64,
    limit: usize,
) -> io::Result<Vec<u64>> {
    let mut cleaned = ChecksumPageReader::new(reader, stored_bytes, 0)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut candidates = Vec::new();
    let mut previous = [0_u8; 2];
    let mut seen = 0_u64;

    loop {
        let count = cleaned.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        for byte in &buffer[..count] {
            if seen >= 2 && previous == GZIP_MAGIC[..2] && *byte == GZIP_MAGIC[2] {
                if candidates.len() >= limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("gzip candidate count exceeds {limit}"),
                    ));
                }
                candidates.push(seen - 2);
            }
            previous[0] = previous[1];
            previous[1] = *byte;
            seen += 1;
        }
    }
    Ok(candidates)
}

enum MemberDecodeFailure {
    Invalid(String),
    Limit(String),
}

struct DecodedMember {
    compressed_bytes: u64,
    decoded_bytes: u64,
    prefix: [u8; PARTITION_MEMBER_PREFIX_BYTES],
    prefix_len: u8,
    class_prefix_counts: BTreeMap<u16, u64>,
    marker_envelopes: Vec<MarkerEnvelope>,
    marker_candidates: u64,
    marker_envelopes_truncated: bool,
}

impl DecodedMember {
    fn into_member(
        self,
        index: usize,
        logical_offset: u64,
        stored_offset: u64,
        descriptor: Option<MemberDescriptor>,
    ) -> PartitionMember {
        PartitionMember {
            index,
            logical_offset,
            stored_offset,
            compressed_bytes: self.compressed_bytes,
            decoded_bytes: self.decoded_bytes,
            decoded_prefix: self.prefix,
            decoded_prefix_len: self.prefix_len,
            class_prefix_candidates: self.class_prefix_counts.values().sum(),
            marker_candidates: self.marker_candidates,
            descriptor,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MemberDecodeOptions {
    limit: u64,
    class_prefix_range: Option<(u16, u16)>,
    marker_envelope: Option<MarkerEnvelopeOptions>,
    member_index: usize,
}

fn decode_member(
    reader: &mut impl Read,
    options: MemberDecodeOptions,
) -> Result<DecodedMember, MemberDecodeFailure> {
    let MemberDecodeOptions {
        limit,
        class_prefix_range,
        marker_envelope,
        member_index,
    } = options;
    let header_bytes = read_gzip_header(reader).map_err(MemberDecodeFailure::Invalid)?;
    let mut decoder = DeflateDecoder::new(reader);
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut decoded_bytes = 0_u64;
    let mut prefix = [0_u8; PARTITION_MEMBER_PREFIX_BYTES];
    let mut prefix_len = 0_usize;
    let mut class_prefix_counter = class_prefix_range.map(ClassPrefixCounter::new);
    let mut envelope_collector =
        marker_envelope.map(|envelope| MarkerEnvelopeCollector::new(envelope, member_index));

    loop {
        let count = decoder
            .read(&mut buffer)
            .map_err(|error| MemberDecodeFailure::Invalid(error.to_string()))?;
        if count == 0 {
            break;
        }
        let prefix_bytes = count.min(PARTITION_MEMBER_PREFIX_BYTES - prefix_len);
        prefix[prefix_len..prefix_len + prefix_bytes].copy_from_slice(&buffer[..prefix_bytes]);
        prefix_len += prefix_bytes;
        if let Some(counter) = &mut class_prefix_counter {
            counter.scan(&buffer[..count]);
        }
        if let Some(collector) = &mut envelope_collector {
            collector.scan(&buffer[..count]);
        }
        decoded_bytes = decoded_bytes.checked_add(count as u64).ok_or_else(|| {
            MemberDecodeFailure::Limit("decoded-byte count overflowed".to_owned())
        })?;
        if decoded_bytes > limit {
            return Err(MemberDecodeFailure::Limit(format!(
                "decoded member exceeds {limit} bytes"
            )));
        }
    }

    let compressed_bytes = (header_bytes as u64)
        .checked_add(decoder.total_in())
        .ok_or_else(|| {
            MemberDecodeFailure::Invalid("compressed-byte count overflowed".to_owned())
        })?;
    if decoder.total_in() == 0 {
        return Err(MemberDecodeFailure::Invalid(
            "empty DEFLATE body".to_owned(),
        ));
    }
    let (marker_envelopes, marker_candidates, marker_envelopes_truncated) = envelope_collector
        .map_or_else(
            || (Vec::new(), 0, false),
            |collector| {
                (
                    collector.envelopes,
                    collector.candidates,
                    collector.truncated,
                )
            },
        );
    Ok(DecodedMember {
        compressed_bytes,
        decoded_bytes,
        prefix,
        prefix_len: u8::try_from(prefix_len).expect("16-byte prefix length fits in u8"),
        class_prefix_counts: class_prefix_counter
            .map_or_else(BTreeMap::new, |counter| counter.counts),
        marker_envelopes,
        marker_candidates,
        marker_envelopes_truncated,
    })
}

/// Captures bounded byte context around every `[class index:u16][zero:u16]`
/// marker while a member is inflated, without retaining the whole payload.
struct MarkerEnvelopeCollector {
    pattern: [u8; MARKER_ENVELOPE_MARKER_BYTES],
    leading_bytes: usize,
    trailing_bytes: usize,
    max_envelopes: usize,
    member_index: usize,
    /// Most recent leading context plus the bytes a marker could end on.
    history: VecDeque<u8>,
    /// Decoded bytes consumed so far in this member.
    position: u64,
    /// Envelopes still collecting trailing bytes, oldest first.
    open: VecDeque<usize>,
    envelopes: Vec<MarkerEnvelope>,
    candidates: u64,
    truncated: bool,
}

impl MarkerEnvelopeCollector {
    fn new(options: MarkerEnvelopeOptions, member_index: usize) -> Self {
        let mut pattern = [0_u8; MARKER_ENVELOPE_MARKER_BYTES];
        pattern[..2].copy_from_slice(&options.class_index.to_le_bytes());
        Self {
            pattern,
            leading_bytes: options.leading_bytes,
            trailing_bytes: options.trailing_bytes,
            max_envelopes: options.max_envelopes,
            member_index,
            history: VecDeque::with_capacity(options.leading_bytes + MARKER_ENVELOPE_MARKER_BYTES),
            position: 0,
            open: VecDeque::new(),
            envelopes: Vec::new(),
            candidates: 0,
            truncated: false,
        }
    }

    fn scan(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.extend_open_envelopes(byte);
            self.history.push_back(byte);
            while self.history.len() > self.leading_bytes + MARKER_ENVELOPE_MARKER_BYTES {
                self.history.pop_front();
            }
            self.position = self.position.saturating_add(1);
            if self.history.len() >= MARKER_ENVELOPE_MARKER_BYTES && self.marker_ends_here() {
                self.open_envelope();
            }
        }
    }

    /// Appends one trailing byte to every envelope that is still short of its
    /// budget. Envelopes complete in the order they were opened.
    fn extend_open_envelopes(&mut self, byte: u8) {
        for &index in &self.open {
            self.envelopes[index].bytes.push(byte);
        }
        while let Some(&index) = self.open.front() {
            if self.envelopes[index].trailing().len() >= self.trailing_bytes {
                self.open.pop_front();
            } else {
                break;
            }
        }
    }

    fn marker_ends_here(&self) -> bool {
        let start = self.history.len() - MARKER_ENVELOPE_MARKER_BYTES;
        self.history.iter().skip(start).eq(self.pattern.iter())
    }

    fn open_envelope(&mut self) {
        self.candidates = self.candidates.saturating_add(1);
        if self.envelopes.len() >= self.max_envelopes {
            self.truncated = true;
            return;
        }

        let bytes = self.history.iter().copied().collect::<Vec<_>>();
        let marker_position = bytes.len() - MARKER_ENVELOPE_MARKER_BYTES;
        let index = self.envelopes.len();
        self.envelopes.push(MarkerEnvelope {
            member_index: self.member_index,
            decoded_offset: self.position - MARKER_ENVELOPE_MARKER_BYTES as u64,
            bytes,
            marker_position,
        });
        if self.trailing_bytes > 0 {
            self.open.push_back(index);
        }
    }
}

struct ClassPrefixCounter {
    first: u16,
    last: u16,
    previous: [u8; 3],
    seen: usize,
    counts: BTreeMap<u16, u64>,
}

impl ClassPrefixCounter {
    fn new((first, last): (u16, u16)) -> Self {
        Self {
            first,
            last,
            previous: [0; 3],
            seen: 0,
            counts: BTreeMap::new(),
        }
    }

    fn scan(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.seen >= 3 {
                let tag = u16::from_le_bytes([self.previous[0], self.previous[1]]);
                if (self.first..=self.last).contains(&tag) && self.previous[2] == 0 && byte == 0 {
                    let count = self.counts.entry(tag).or_default();
                    *count = count.saturating_add(1);
                }
            }
            self.previous.rotate_left(1);
            self.previous[2] = byte;
            self.seen = self.seen.saturating_add(1);
        }
    }
}

fn read_gzip_header(reader: &mut impl Read) -> Result<usize, String> {
    let mut fixed = [0_u8; GZIP_FIXED_HEADER_LEN];
    reader
        .read_exact(&mut fixed)
        .map_err(|error| format!("truncated gzip header: {error}"))?;
    if fixed[..3] != GZIP_MAGIC {
        return Err("gzip magic or compression method is invalid".to_owned());
    }

    let flags = fixed[3];
    if flags & GZIP_FLAG_RESERVED != 0 {
        return Err("gzip reserved flag bits are set".to_owned());
    }
    let mut consumed = GZIP_FIXED_HEADER_LEN;

    if flags & GZIP_FLAG_EXTRA != 0 {
        let mut length = [0_u8; 2];
        reader
            .read_exact(&mut length)
            .map_err(|error| format!("truncated gzip extra length: {error}"))?;
        let length = usize::from(u16::from_le_bytes(length));
        consumed = checked_header_add(consumed, 2 + length)?;
        discard_exact(reader, length, "gzip extra field")?;
    }
    if flags & GZIP_FLAG_NAME != 0 {
        consumed = read_zero_terminated(reader, consumed, "gzip file name")?;
    }
    if flags & GZIP_FLAG_COMMENT != 0 {
        consumed = read_zero_terminated(reader, consumed, "gzip comment")?;
    }
    if flags & GZIP_FLAG_HEADER_CRC != 0 {
        consumed = checked_header_add(consumed, 2)?;
        discard_exact(reader, 2, "gzip header CRC")?;
    }
    Ok(consumed)
}

fn checked_header_add(consumed: usize, additional: usize) -> Result<usize, String> {
    let total = consumed
        .checked_add(additional)
        .ok_or_else(|| "gzip header length overflowed".to_owned())?;
    if total > MAX_GZIP_HEADER_BYTES {
        return Err(format!("gzip header exceeds {MAX_GZIP_HEADER_BYTES} bytes"));
    }
    Ok(total)
}

fn discard_exact(reader: &mut impl Read, mut length: usize, what: &str) -> Result<(), String> {
    let mut buffer = [0_u8; 4096];
    while length > 0 {
        let wanted = length.min(buffer.len());
        reader
            .read_exact(&mut buffer[..wanted])
            .map_err(|error| format!("truncated {what}: {error}"))?;
        length -= wanted;
    }
    Ok(())
}

fn read_zero_terminated(
    reader: &mut impl Read,
    mut consumed: usize,
    what: &str,
) -> Result<usize, String> {
    loop {
        consumed = checked_header_add(consumed, 1)?;
        let mut byte = [0_u8; 1];
        reader
            .read_exact(&mut byte)
            .map_err(|error| format!("truncated {what}: {error}"))?;
        if byte[0] == 0 {
            return Ok(consumed);
        }
    }
}

fn logical_to_stored_offset(logical_offset: u64, stored_bytes: u64) -> u64 {
    let full_pages = stored_bytes / REVIT_STORED_PAGE_BYTES as u64;
    let full_payload_bytes = full_pages * REVIT_PAGE_PAYLOAD_BYTES as u64;
    if logical_offset >= full_payload_bytes {
        return full_pages * REVIT_STORED_PAGE_BYTES as u64 + (logical_offset - full_payload_bytes);
    }
    let page = logical_offset / REVIT_PAGE_PAYLOAD_BYTES as u64;
    let within_page = logical_offset % REVIT_PAGE_PAYLOAD_BYTES as u64;
    page * REVIT_STORED_PAGE_BYTES as u64 + within_page
}

struct ChecksumPageReader<'a, R> {
    inner: &'a mut R,
    logical_position: u64,
    logical_length: u64,
    full_payload_bytes: u64,
}

impl<'a, R: Read + Seek> ChecksumPageReader<'a, R> {
    fn new(inner: &'a mut R, stored_bytes: u64, logical_offset: u64) -> io::Result<Self> {
        let full_pages = stored_bytes / REVIT_STORED_PAGE_BYTES as u64;
        let logical_length = stored_bytes - full_pages * REVIT_PAGE_CHECKSUM_BYTES as u64;
        if logical_offset > logical_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "logical offset exceeds checksum-clean stream length",
            ));
        }
        let stored_offset = logical_to_stored_offset(logical_offset, stored_bytes);
        inner.seek(SeekFrom::Start(stored_offset))?;
        Ok(Self {
            inner,
            logical_position: logical_offset,
            logical_length,
            full_payload_bytes: full_pages * REVIT_PAGE_PAYLOAD_BYTES as u64,
        })
    }
}

impl<R: Read + Seek> Read for ChecksumPageReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.logical_position >= self.logical_length {
            return Ok(0);
        }

        let available = if self.logical_position < self.full_payload_bytes {
            REVIT_PAGE_PAYLOAD_BYTES as u64
                - self.logical_position % REVIT_PAGE_PAYLOAD_BYTES as u64
        } else {
            self.logical_length - self.logical_position
        };
        let wanted = output
            .len()
            .min(usize::try_from(available).unwrap_or(usize::MAX));
        let count = self.inner.read(&mut output[..wanted])?;
        self.logical_position += count as u64;

        if count > 0
            && self.logical_position <= self.full_payload_bytes
            && self.logical_position % REVIT_PAGE_PAYLOAD_BYTES as u64 == 0
        {
            self.inner.seek(SeekFrom::Current(
                i64::try_from(REVIT_PAGE_CHECKSUM_BYTES).expect("checksum page length fits in i64"),
            ))?;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use flate2::{Compression, write::DeflateEncoder};

    use super::*;

    fn truncated_gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).unwrap();
        let deflate = encoder.finish().unwrap();
        let mut bytes = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        bytes.extend(deflate);
        bytes
    }

    #[test]
    fn maps_offsets_across_checksum_pages() {
        let stored = 2 * REVIT_STORED_PAGE_BYTES as u64 + 17;
        assert_eq!(logical_to_stored_offset(0, stored), 0);
        assert_eq!(
            logical_to_stored_offset(REVIT_PAGE_PAYLOAD_BYTES as u64, stored),
            REVIT_STORED_PAGE_BYTES as u64
        );
        assert_eq!(
            logical_to_stored_offset(2 * REVIT_PAGE_PAYLOAD_BYTES as u64 + 7, stored),
            2 * REVIT_STORED_PAGE_BYTES as u64 + 7
        );
    }

    #[test]
    fn reads_payload_and_skips_checksum_bytes() {
        let mut stored = vec![0x11; REVIT_PAGE_PAYLOAD_BYTES];
        stored.extend(vec![0xaa; REVIT_PAGE_CHECKSUM_BYTES]);
        stored.extend([0x22; 17]);
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let mut cleaned = ChecksumPageReader::new(&mut cursor, stored_len, 0).unwrap();
        let mut output = Vec::new();
        cleaned.read_to_end(&mut output).unwrap();
        assert_eq!(output.len(), REVIT_PAGE_PAYLOAD_BYTES + 17);
        assert!(
            output[..REVIT_PAGE_PAYLOAD_BYTES]
                .iter()
                .all(|byte| *byte == 0x11)
        );
        assert_eq!(&output[REVIT_PAGE_PAYLOAD_BYTES..], &[0x22; 17]);
    }

    #[test]
    fn inventories_concatenated_members_without_retaining_payloads() {
        let mut stored = vec![0; 44];
        stored.extend(truncated_gzip(b"first"));
        stored.extend(truncated_gzip(b"second payload"));
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let report = analyze_partition(
            "Partitions/1".to_owned(),
            &mut cursor,
            stored_len,
            PartitionReadOptions::default(),
        )
        .unwrap();

        assert_eq!(report.members.len(), 2);
        assert_eq!(report.members[0].logical_offset, 44);
        assert_eq!(report.members[0].decoded_bytes, 5);
        assert_eq!(report.members[0].decoded_prefix(), b"first");
        assert_eq!(report.members[1].decoded_bytes, 14);
        assert_eq!(report.members[1].decoded_prefix(), b"second payload");
        assert_eq!(report.total_decoded_bytes, 19);
        assert!(report.class_prefix_counts.is_empty());
        assert!(report.failures.is_empty());
        assert!(!report.truncated_by_limit);
    }

    #[test]
    fn enforces_gzip_candidate_bound() {
        let stored = [GZIP_MAGIC, GZIP_MAGIC].concat();
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let error = analyze_partition(
            "Partitions/1".to_owned(),
            &mut cursor,
            stored_len,
            PartitionReadOptions {
                max_candidates: 1,
                ..PartitionReadOptions::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("candidate count exceeds 1"));
    }

    #[test]
    fn stops_at_member_decode_bound() {
        let stored = truncated_gzip(&[0; 32]);
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let report = analyze_partition(
            "Partitions/1".to_owned(),
            &mut cursor,
            stored_len,
            PartitionReadOptions {
                max_member_decoded_bytes: 16,
                ..PartitionReadOptions::default()
            },
        )
        .unwrap();
        assert!(report.members.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.truncated_by_limit);
        assert!(report.failures[0].message.contains("exceeds 16 bytes"));
    }

    #[test]
    fn parses_the_descriptor_in_front_of_a_member() {
        let member = truncated_gzip(b"member payload");
        let mut descriptor = vec![0_u8; MEMBER_DESCRIPTOR_BYTES];
        descriptor[4..8].copy_from_slice(&4096_u32.to_le_bytes());
        descriptor[8..10].copy_from_slice(&0x0e47_u16.to_le_bytes());
        descriptor[10..14].copy_from_slice(&512_u32.to_le_bytes());
        descriptor[14..16].copy_from_slice(&0x0e4e_u16.to_le_bytes());
        descriptor[16..18].copy_from_slice(&4_u16.to_le_bytes());
        descriptor[20..24].copy_from_slice(&7_u32.to_le_bytes());
        descriptor[24..28]
            .copy_from_slice(&(u32::try_from(member.len()).unwrap() + 16).to_le_bytes());
        descriptor[28..32].copy_from_slice(&96_u32.to_le_bytes());
        descriptor[32..36].copy_from_slice(&102_u32.to_le_bytes());

        let mut stored = descriptor.clone();
        stored.extend(member);
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let report = analyze_partition(
            "Partitions/1".to_owned(),
            &mut cursor,
            stored_len,
            PartitionReadOptions::default(),
        )
        .unwrap();

        let parsed = report.members[0].descriptor.unwrap();
        assert_eq!(parsed.previous_decoded_bytes, 4096);
        assert_eq!(parsed.previous_tag, 0x0e47);
        assert_eq!(parsed.previous_stored_span, 512);
        assert_eq!(parsed.tag, 0x0e4e);
        assert_eq!(parsed.flags, 4);
        assert_eq!(parsed.record_count, 7);
        assert_eq!(parsed.record_body_bytes, 96);
        assert_eq!(parsed.format_tag, 102);
        assert_eq!(parsed.bytes().as_slice(), descriptor.as_slice());
        assert!(parsed.matches_stored_span(report.members[0].compressed_bytes));
    }

    #[test]
    fn reports_no_descriptor_when_the_member_starts_the_stream() {
        let stored = truncated_gzip(b"member payload");
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let report = analyze_partition(
            "Partitions/1".to_owned(),
            &mut cursor,
            stored_len,
            PartitionReadOptions::default(),
        )
        .unwrap();
        assert!(report.members[0].descriptor.is_none());
    }

    #[test]
    fn inflates_one_member_by_offset() {
        let mut stored = vec![0; 40];
        stored.extend(truncated_gzip(b"member payload"));
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let payload = decode_member_bytes(&mut cursor, stored_len, 40, 1024).unwrap();
        assert_eq!(payload, b"member payload");
    }

    #[test]
    fn refuses_a_member_above_the_decode_budget() {
        let mut stored = vec![0; 40];
        stored.extend(truncated_gzip(&[0; 64]));
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        let error = decode_member_bytes(&mut cursor, stored_len, 40, 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds 16 bytes"));
    }

    const MARKER_CLASS_INDEX: u16 = 401;
    const MARKER_BYTES: [u8; MARKER_ENVELOPE_MARKER_BYTES] = [0x91, 0x01, 0x00, 0x00];

    fn envelope_options(leading: usize, trailing: usize, max: usize) -> PartitionReadOptions {
        PartitionReadOptions {
            marker_envelope: Some(MarkerEnvelopeOptions {
                class_index: MARKER_CLASS_INDEX,
                leading_bytes: leading,
                trailing_bytes: trailing,
                max_envelopes: max,
            }),
            ..PartitionReadOptions::default()
        }
    }

    fn analyze_single_member(
        payload: &[u8],
        options: PartitionReadOptions,
    ) -> io::Result<PartitionReport> {
        let stored = truncated_gzip(payload);
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);
        analyze_partition("Partitions/1".to_owned(), &mut cursor, stored_len, options)
    }

    #[test]
    fn captures_marker_envelopes_with_decoded_offsets() {
        let mut payload = vec![0x7f; 64];
        payload[16..20].copy_from_slice(&MARKER_BYTES);
        payload[40..44].copy_from_slice(&MARKER_BYTES);

        let report = analyze_single_member(&payload, envelope_options(8, 4, 8)).unwrap();

        assert_eq!(report.marker_candidates, 2);
        assert_eq!(report.members[0].marker_candidates, 2);
        assert!(!report.marker_envelopes_truncated);
        assert_eq!(report.marker_envelopes.len(), 2);

        let first = &report.marker_envelopes[0];
        assert_eq!(first.member_index, 0);
        assert_eq!(first.decoded_offset, 16);
        assert_eq!(first.leading(), [0x7f; 8]);
        assert_eq!(first.marker(), MARKER_BYTES);
        assert_eq!(first.trailing(), [0x7f; 4]);
        assert_eq!(first.bytes().len(), 16);
        assert_eq!(report.marker_envelopes[1].decoded_offset, 40);
    }

    #[test]
    fn clamps_marker_context_at_member_boundaries() {
        let mut payload = vec![0x7f; 12];
        payload[2..6].copy_from_slice(&MARKER_BYTES);
        payload[8..12].copy_from_slice(&MARKER_BYTES);

        let report = analyze_single_member(&payload, envelope_options(8, 4, 8)).unwrap();

        assert_eq!(report.marker_envelopes.len(), 2);
        assert_eq!(report.marker_envelopes[0].decoded_offset, 2);
        assert_eq!(report.marker_envelopes[0].leading(), [0x7f; 2]);
        // Context windows may overlap: the tail of the first envelope already
        // contains the start of the next marker.
        assert_eq!(
            report.marker_envelopes[0].trailing(),
            [0x7f, 0x7f, 0x91, 0x01]
        );
        assert_eq!(report.marker_envelopes[1].decoded_offset, 8);
        assert_eq!(
            report.marker_envelopes[1].leading(),
            [0x7f, 0x7f, 0x91, 0x01, 0x00, 0x00, 0x7f, 0x7f]
        );
        assert!(report.marker_envelopes[1].trailing().is_empty());
    }

    #[test]
    fn captures_markers_spanning_inflate_read_boundaries() {
        let marker_offset = 65_534_usize;
        let mut payload = vec![0x7f; marker_offset + 64];
        payload[marker_offset..marker_offset + MARKER_ENVELOPE_MARKER_BYTES]
            .copy_from_slice(&MARKER_BYTES);

        let report = analyze_single_member(&payload, envelope_options(8, 8, 8)).unwrap();

        assert_eq!(report.marker_envelopes.len(), 1);
        let envelope = &report.marker_envelopes[0];
        assert_eq!(envelope.decoded_offset, marker_offset as u64);
        assert_eq!(envelope.leading(), [0x7f; 8]);
        assert_eq!(envelope.marker(), MARKER_BYTES);
        assert_eq!(envelope.trailing(), [0x7f; 8]);
    }

    #[test]
    fn counts_candidates_beyond_the_envelope_budget() {
        let mut payload = vec![0x7f; 64];
        payload[16..20].copy_from_slice(&MARKER_BYTES);
        payload[40..44].copy_from_slice(&MARKER_BYTES);

        let report = analyze_single_member(&payload, envelope_options(8, 4, 1)).unwrap();

        assert_eq!(report.marker_candidates, 2);
        assert_eq!(report.marker_envelopes.len(), 1);
        assert_eq!(report.marker_envelopes[0].decoded_offset, 16);
        assert!(report.marker_envelopes_truncated);
    }

    #[test]
    fn spends_the_envelope_budget_across_members() {
        let mut payload = vec![0x7f; 32];
        payload[16..20].copy_from_slice(&MARKER_BYTES);
        let mut stored = truncated_gzip(&payload);
        stored.extend(truncated_gzip(&payload));
        let stored_len = stored.len() as u64;
        let mut cursor = Cursor::new(stored);

        let report = analyze_partition(
            "Partitions/1".to_owned(),
            &mut cursor,
            stored_len,
            envelope_options(4, 4, 1),
        )
        .unwrap();

        assert_eq!(report.members.len(), 2);
        assert_eq!(report.marker_candidates, 2);
        assert_eq!(report.marker_envelopes.len(), 1);
        assert_eq!(report.marker_envelopes[0].member_index, 0);
        assert!(report.marker_envelopes_truncated);
    }

    #[test]
    fn rejects_oversized_marker_context() {
        let error = analyze_single_member(
            b"payload",
            envelope_options(MAX_MARKER_ENVELOPE_CONTEXT_BYTES + 1, 0, 1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("bytes per side"));
    }

    #[test]
    fn counts_schema_prefixes_across_input_slices() {
        let mut counter = ClassPrefixCounter::new((400, 402));
        counter.scan(&[0xaa, 0x91]);
        counter.scan(&[0x01, 0x00]);
        counter.scan(&[0x00, 0x92, 0x01, 0x00, 0x01]);
        assert_eq!(counter.counts.get(&401), Some(&1));
        assert!(!counter.counts.contains_key(&402));
    }
}
