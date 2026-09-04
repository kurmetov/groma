use std::{collections::BTreeSet, fmt};

const COMMON_HEADER_BYTES: usize = 16;
const MARKER_SCAN_BYTES: usize = 512;
const FAMILY_RECORD_START: usize = 0x30;
const FAMILY_RECORD_STRIDE: usize = 12;
const EXPLICIT_MARKERS_TO_VALIDATE: usize = 8;

/// Counts declared by the decoded `Global/ElemTable` header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElemTableHeader {
    pub element_count: u16,
    pub record_count: u16,
}

/// How records are delimited in the decoded table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordFraming {
    /// Observed in public family-file samples.
    Implicit,
    /// A sentinel field repeats inside every project-file record.
    Explicit { marker_bytes: usize },
}

/// Physical record layout recovered without assigning BIM semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElemTableLayout {
    pub start: usize,
    pub stride: usize,
    pub marker_offset: usize,
    pub framing: RecordFraming,
}

/// One record in the decoded table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElemTableRecord {
    pub offset: usize,
    /// Candidate element identifier, as observed in public project corpora.
    pub id_primary: u32,
    /// Repeated/correlated identifier field; its full semantics remain unknown.
    pub id_secondary: u32,
}

/// Loss-preserving parse of `Global/ElemTable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElemTable {
    pub header: ElemTableHeader,
    pub layout: ElemTableLayout,
    pub records: Vec<ElemTableRecord>,
    decoded: Vec<u8>,
    records_end: usize,
}

impl ElemTable {
    /// Parse a decoded `Global/ElemTable` using only corpus-backed layouts.
    ///
    /// The parser validates an initial run of explicit layout markers. Corpus
    /// evidence shows the same field can take non-sentinel values later, so it
    /// is not treated as a delimiter for every record. A declared record count
    /// larger than the available complete record array is preserved as a
    /// visible count mismatch instead of being silently repaired.
    ///
    /// # Errors
    ///
    /// Returns an error for a short header, an unsupported layout, overflow,
    /// or a broken marker inside the recoverable record array.
    pub fn parse(decoded: &[u8]) -> Result<Self, ElemTableError> {
        if decoded.len() < COMMON_HEADER_BYTES {
            return Err(ElemTableError::HeaderTooShort {
                actual: decoded.len(),
            });
        }

        let header = ElemTableHeader {
            element_count: read_u16(decoded, 0),
            record_count: read_u16(decoded, 2),
        };
        let layout = detect_layout(decoded, usize::from(header.record_count))?;
        let records = parse_records(decoded, header.record_count, layout)?;
        let records_end = records
            .last()
            .map_or(layout.start, |record| record.offset + layout.stride);

        Ok(Self {
            header,
            layout,
            records,
            decoded: decoded.to_vec(),
            records_end,
        })
    }

    #[must_use]
    pub fn decoded_bytes(&self) -> &[u8] {
        &self.decoded
    }

    #[must_use]
    pub fn leading_bytes(&self) -> &[u8] {
        &self.decoded[..self.layout.start]
    }

    #[must_use]
    pub fn trailing_bytes(&self) -> &[u8] {
        &self.decoded[self.records_end..]
    }

    #[must_use]
    pub fn record_bytes(&self, record: &ElemTableRecord) -> &[u8] {
        &self.decoded[record.offset..record.offset + self.layout.stride]
    }

    #[must_use]
    pub fn unique_primary_id_count(&self) -> usize {
        self.records
            .iter()
            .map(|record| record.id_primary)
            .collect::<BTreeSet<_>>()
            .len()
    }

    #[must_use]
    pub fn primary_secondary_mismatch_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.id_primary != record.id_secondary)
            .count()
    }

    /// Count records whose layout-marker field currently holds all `0xff`.
    #[must_use]
    pub fn marker_match_count(&self) -> usize {
        let RecordFraming::Explicit { marker_bytes } = self.layout.framing else {
            return 0;
        };
        self.records
            .iter()
            .filter(|record| {
                let marker = record.offset + self.layout.marker_offset;
                self.decoded[marker..marker + marker_bytes]
                    .iter()
                    .all(|byte| *byte == 0xff)
            })
            .count()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElemTableError {
    HeaderTooShort { actual: usize },
    UnsupportedLayout,
    RecordSpanOverflow,
}

impl fmt::Display for ElemTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooShort { actual } => write!(
                formatter,
                "Global/ElemTable header needs at least {COMMON_HEADER_BYTES} bytes, got {actual}"
            ),
            Self::UnsupportedLayout => formatter.write_str(
                "Global/ElemTable does not match a verified 12-, 28-, or 40-byte record layout",
            ),
            Self::RecordSpanOverflow => {
                formatter.write_str("Global/ElemTable record span overflows address space")
            }
        }
    }
}

impl std::error::Error for ElemTableError {}

fn detect_layout(
    decoded: &[u8],
    declared_records: usize,
) -> Result<ElemTableLayout, ElemTableError> {
    if let Some(layout) = detect_explicit_layout(decoded, declared_records, 8, 40) {
        return Ok(layout);
    }
    if let Some(layout) = detect_explicit_layout(decoded, declared_records, 4, 28) {
        return Ok(layout);
    }

    let minimum_end = FAMILY_RECORD_START
        .checked_add(
            declared_records
                .checked_mul(FAMILY_RECORD_STRIDE)
                .ok_or(ElemTableError::RecordSpanOverflow)?,
        )
        .ok_or(ElemTableError::RecordSpanOverflow)?;
    if decoded.len() >= minimum_end && !has_ff_marker(decoded) {
        return Ok(ElemTableLayout {
            start: FAMILY_RECORD_START,
            stride: FAMILY_RECORD_STRIDE,
            marker_offset: 0,
            framing: RecordFraming::Implicit,
        });
    }

    Err(ElemTableError::UnsupportedLayout)
}

fn detect_explicit_layout(
    decoded: &[u8],
    declared_records: usize,
    marker_bytes: usize,
    stride: usize,
) -> Option<ElemTableLayout> {
    let first_marker = find_marker(decoded, marker_bytes)?;
    let candidate_offsets: &[usize] = if marker_bytes == 8 { &[4, 0] } else { &[0] };

    for &marker_offset in candidate_offsets {
        let Some(start) = first_marker.checked_sub(marker_offset) else {
            continue;
        };
        if start % 2 != 0 {
            continue;
        }
        let available = decoded.len().saturating_sub(start) / stride;
        let records_to_validate = declared_records
            .min(available)
            .min(EXPLICIT_MARKERS_TO_VALIDATE);
        if records_to_validate == 0 {
            continue;
        }
        let all_markers_match = (0..records_to_validate).all(|record| {
            let marker = start + record * stride + marker_offset;
            decoded
                .get(marker..marker + marker_bytes)
                .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0xff))
        });
        if all_markers_match {
            return Some(ElemTableLayout {
                start,
                stride,
                marker_offset,
                framing: RecordFraming::Explicit { marker_bytes },
            });
        }
    }
    None
}

fn find_marker(decoded: &[u8], marker_bytes: usize) -> Option<usize> {
    let end = decoded.len().min(MARKER_SCAN_BYTES);
    decoded
        .get(COMMON_HEADER_BYTES..end)?
        .windows(marker_bytes)
        .position(|window| window.iter().all(|byte| *byte == 0xff))
        .map(|relative| COMMON_HEADER_BYTES + relative)
}

fn has_ff_marker(decoded: &[u8]) -> bool {
    find_marker(decoded, 4).is_some()
}

fn parse_records(
    decoded: &[u8],
    declared_count: u16,
    layout: ElemTableLayout,
) -> Result<Vec<ElemTableRecord>, ElemTableError> {
    let available_records = decoded.len().saturating_sub(layout.start) / layout.stride;
    let count = usize::from(declared_count).min(available_records);
    let mut records = Vec::with_capacity(count);

    for index in 0..count {
        let offset = layout
            .start
            .checked_add(
                index
                    .checked_mul(layout.stride)
                    .ok_or(ElemTableError::RecordSpanOverflow)?,
            )
            .ok_or(ElemTableError::RecordSpanOverflow)?;
        let (id_primary_offset, id_secondary_offset) = match layout.framing {
            RecordFraming::Implicit => (offset, offset + 4),
            RecordFraming::Explicit { marker_bytes } => {
                let marker = offset + layout.marker_offset;
                if layout.stride == 40 {
                    (offset + 16, offset + 36)
                } else {
                    let body = marker + marker_bytes;
                    (body, body + 4)
                }
            }
        };
        records.push(ElemTableRecord {
            offset,
            id_primary: read_u32(decoded, id_primary_offset),
            id_secondary: read_u32(decoded, id_secondary_offset),
        });
    }
    Ok(records)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn explicit_2023_fixture(declared: u16, complete: usize, tail: usize) -> Vec<u8> {
        let mut bytes = vec![0; 0x1e + complete * 28 + tail];
        bytes[..2].copy_from_slice(&declared.to_le_bytes());
        bytes[2..4].copy_from_slice(&declared.to_le_bytes());
        for index in 0..complete {
            let offset = 0x1e + index * 28;
            bytes[offset..offset + 4].fill(0xff);
            let id = u32::try_from(index + 1).unwrap();
            bytes[offset + 4..offset + 8].copy_from_slice(&id.to_le_bytes());
            bytes[offset + 8..offset + 12].copy_from_slice(&id.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_explicit_2023_records_and_preserves_tail() {
        let bytes = explicit_2023_fixture(3, 3, 7);
        let table = ElemTable::parse(&bytes).unwrap();

        assert_eq!(table.layout.start, 0x1e);
        assert_eq!(table.layout.stride, 28);
        assert_eq!(table.records.len(), 3);
        assert_eq!(table.records[2].id_primary, 3);
        assert_eq!(table.trailing_bytes().len(), 7);
        assert_eq!(table.record_bytes(&table.records[0]).len(), 28);
    }

    #[test]
    fn reports_declared_record_shortfall_without_guessing_a_record() {
        let bytes = explicit_2023_fixture(4, 3, 5);
        let table = ElemTable::parse(&bytes).unwrap();
        assert_eq!(table.header.record_count, 4);
        assert_eq!(table.records.len(), 3);
        assert_eq!(table.trailing_bytes().len(), 5);
    }

    #[test]
    fn recovers_2024_record_origin_before_marker() {
        let mut bytes = vec![0; 0x1e + 3 * 40];
        bytes[..2].copy_from_slice(&3_u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&3_u16.to_le_bytes());
        for index in 0..3 {
            let offset = 0x1e + index * 40;
            bytes[offset + 4..offset + 12].fill(0xff);
            let id = u32::try_from(index + 1).unwrap();
            bytes[offset + 16..offset + 20].copy_from_slice(&id.to_le_bytes());
            bytes[offset + 36..offset + 40].copy_from_slice(&id.to_le_bytes());
        }

        let table = ElemTable::parse(&bytes).unwrap();
        assert_eq!(table.layout.start, 0x1e);
        assert_eq!(table.layout.marker_offset, 4);
        assert_eq!(table.layout.stride, 40);
        assert_eq!(table.records.len(), 3);
        assert!(table.trailing_bytes().is_empty());
    }

    #[test]
    fn parses_implicit_family_records() {
        let mut bytes = vec![0; FAMILY_RECORD_START + 2 * FAMILY_RECORD_STRIDE];
        bytes[..2].copy_from_slice(&2_u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&2_u16.to_le_bytes());
        bytes[FAMILY_RECORD_START..FAMILY_RECORD_START + 4].copy_from_slice(&7_u32.to_le_bytes());
        bytes[FAMILY_RECORD_START + 4..FAMILY_RECORD_START + 8]
            .copy_from_slice(&7_u32.to_le_bytes());

        let table = ElemTable::parse(&bytes).unwrap();
        assert_eq!(table.layout.framing, RecordFraming::Implicit);
        assert_eq!(table.records.len(), 2);
        assert_eq!(table.records[0].id_primary, 7);
    }

    #[test]
    fn rejects_unknown_layout() {
        let bytes = vec![0; COMMON_HEADER_BYTES];
        assert_eq!(
            ElemTable::parse(&bytes),
            Err(ElemTableError::UnsupportedLayout)
        );
    }

    #[test]
    fn counts_unique_and_mismatched_ids() {
        let mut bytes = explicit_2023_fixture(2, 2, 0);
        bytes[0x1e + 28 + 4..0x1e + 28 + 8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[0x1e + 28 + 8..0x1e + 28 + 12].copy_from_slice(&9_u32.to_le_bytes());
        let table = ElemTable::parse(&bytes).unwrap();
        assert_eq!(table.unique_primary_id_count(), 1);
        assert_eq!(table.primary_secondary_mismatch_count(), 1);
    }

    #[test]
    fn later_non_sentinel_values_do_not_break_a_detected_layout() {
        let mut bytes = explicit_2023_fixture(10, 10, 0);
        let ninth_marker = 0x1e + 8 * 28;
        bytes[ninth_marker..ninth_marker + 4].copy_from_slice(&17_u32.to_le_bytes());

        let table = ElemTable::parse(&bytes).unwrap();
        assert_eq!(table.records.len(), 10);
        assert_eq!(table.marker_match_count(), 9);
    }
}
