//! `Global/ElemTable`: every element the document holds, and when each was
//! created and last changed.
//!
//! The layout is the schema's own. `ElemTable` declares
//! `m_elemArr: [ElemRec]`, and `ElemRec` declares `m_id: ElementId`,
//! `m_history: ElementHistory`, `m_partitionId: PartitionId` and
//! `m_OwningElementId: ElementId`, where `ElementHistory` is
//! `m_originalElementId` plus three `EpisodeId`s - creation, last
//! modification, last user modification. Every one of those is a four-byte
//! identifier, so a record is 28 bytes with nothing variable in it, and the
//! payload is a two-byte class-index tag, a four-byte count, and the array.
//!
//! Verified by walking the stream against those declarations with
//! `groma global FILE Global/ElemTable --class ElemTable --skip 2`, which
//! consumes all but the trailing 8 bytes on all 28 project files available -
//! the four corpus files and the 24 AR/KJ models beside them.
//!
//! This replaces a heuristic that scanned for a run of `0xff` bytes and read
//! the record array from there. That marker is real - it is
//! `m_OwningElementId` on an element nothing owns - but it sits at the *end*
//! of a record, so the scan started the array 24 bytes late and read every
//! field one slot over. It also read the leading class-index tag as a
//! `u16` element count (1370, `ElemTable`'s own class index, reported as
//! "declared elements") and the low half of the record count as the count
//! itself (2847 of 265 503 on AR S1).

use std::{collections::BTreeSet, fmt};

/// The two-byte class tag, then the four-byte count.
const HEADER_BYTES: usize = 6;
/// `m_id`, `m_originalElementId`, three `EpisodeId`s, `m_partitionId`,
/// `m_OwningElementId`.
const RECORD_BYTES: usize = 28;
/// How many of a declared count may be missing from the payload before the
/// parse is treated as a layout failure rather than a truncated table.
const RECORD_SHORTFALL_TOLERANCE: usize = 0;

/// One `ElemRec`, field for field.
///
/// The three episodes are `EpisodeId.m_id` values. They index
/// [`crate::EpisodeTable`] from its *end* - see
/// [`crate::EpisodeTable::position_of`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElemTableRecord {
    pub offset: usize,
    pub id: u32,
    /// `ElementHistory.m_originalElementId` - the same value as `id` on an
    /// element this document authored.
    pub original_id: u32,
    pub creation_episode: i32,
    pub last_modification_episode: i32,
    pub last_user_modification_episode: i32,
    pub partition_id: i32,
    /// `-1` where nothing owns the element, which is most of them.
    pub owning_element_id: i32,
}

/// Loss-preserving parse of `Global/ElemTable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElemTable {
    /// The payload's leading two bytes: the schema class index of the object
    /// it holds, which is `ElemTable`'s own.
    pub class_index: u16,
    /// The count the payload declares, before any of it is read.
    pub declared_records: u32,
    pub records: Vec<ElemTableRecord>,
    decoded: Vec<u8>,
    records_end: usize,
}

impl ElemTable {
    /// Parse a decoded `Global/ElemTable`.
    ///
    /// # Errors
    ///
    /// Returns an error for a payload too short to hold the header, for a
    /// count that overflows, and for a count the payload cannot hold - the
    /// last of which is what a layout this does not understand looks like,
    /// and is reported rather than repaired.
    pub fn parse(decoded: &[u8]) -> Result<Self, ElemTableError> {
        if decoded.len() < HEADER_BYTES {
            return Err(ElemTableError::HeaderTooShort {
                actual: decoded.len(),
            });
        }
        let class_index = read_u16(decoded, 0);
        let declared_records = read_u32(decoded, 2);
        let declared = usize::try_from(declared_records)
            .ok()
            .ok_or(ElemTableError::RecordSpanOverflow)?;
        let span = declared
            .checked_mul(RECORD_BYTES)
            .and_then(|span| span.checked_add(HEADER_BYTES))
            .ok_or(ElemTableError::RecordSpanOverflow)?;
        let available = decoded.len().saturating_sub(HEADER_BYTES) / RECORD_BYTES;
        if declared.saturating_sub(available) > RECORD_SHORTFALL_TOLERANCE {
            return Err(ElemTableError::RecordsDoNotFit {
                declared,
                available,
            });
        }

        let records = (0..declared.min(available))
            .map(|index| {
                let offset = HEADER_BYTES + index * RECORD_BYTES;
                ElemTableRecord {
                    offset,
                    id: read_u32(decoded, offset),
                    original_id: read_u32(decoded, offset + 4),
                    creation_episode: read_i32(decoded, offset + 8),
                    last_modification_episode: read_i32(decoded, offset + 12),
                    last_user_modification_episode: read_i32(decoded, offset + 16),
                    partition_id: read_i32(decoded, offset + 20),
                    owning_element_id: read_i32(decoded, offset + 24),
                }
            })
            .collect::<Vec<_>>();

        Ok(Self {
            class_index,
            declared_records,
            records,
            decoded: decoded.to_vec(),
            records_end: span.min(decoded.len()),
        })
    }

    #[must_use]
    pub fn decoded_bytes(&self) -> &[u8] {
        &self.decoded
    }

    #[must_use]
    pub fn leading_bytes(&self) -> &[u8] {
        &self.decoded[..HEADER_BYTES.min(self.decoded.len())]
    }

    /// What follows the record array: the graveyard records and whatever the
    /// declarations after them hold, kept rather than discarded.
    #[must_use]
    pub fn trailing_bytes(&self) -> &[u8] {
        &self.decoded[self.records_end..]
    }

    #[must_use]
    pub fn record_bytes(&self, record: &ElemTableRecord) -> &[u8] {
        &self.decoded[record.offset..record.offset + RECORD_BYTES]
    }

    #[must_use]
    pub fn unique_id_count(&self) -> usize {
        self.records
            .iter()
            .map(|record| record.id)
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Records whose `m_originalElementId` is not their own id: an element
    /// this document did not author, copied in from somewhere that did.
    #[must_use]
    pub fn copied_in_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.id != record.original_id)
            .count()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElemTableError {
    HeaderTooShort { actual: usize },
    RecordSpanOverflow,
    RecordsDoNotFit { declared: usize, available: usize },
}

impl fmt::Display for ElemTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooShort { actual } => write!(
                formatter,
                "Global/ElemTable header needs at least {HEADER_BYTES} bytes, got {actual}"
            ),
            Self::RecordSpanOverflow => {
                formatter.write_str("Global/ElemTable record span overflows address space")
            }
            Self::RecordsDoNotFit {
                declared,
                available,
            } => write!(
                formatter,
                "Global/ElemTable declares {declared} records and the payload holds {available}"
            ),
        }
    }
}

impl std::error::Error for ElemTableError {}

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

#[allow(clippy::cast_possible_wrap)] // An identifier field, read as declared.
fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    read_u32(bytes, offset) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One table: a class tag, a count, and `records.len()` records of the
    /// declared shape, with `tail` bytes of anything after them.
    fn fixture(declared: u32, records: &[[i32; 7]], tail: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1370_u16.to_le_bytes());
        bytes.extend_from_slice(&declared.to_le_bytes());
        for record in records {
            for field in record {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
        }
        bytes.extend(std::iter::repeat_n(0_u8, tail));
        bytes
    }

    #[test]
    fn reads_every_declared_field_of_a_record() {
        let table = ElemTable::parse(&fixture(
            2,
            &[[7, 7, 1072, 2007, 2007, 13, -1], [9, 4, 0, 438, 438, 2, 7]],
            8,
        ))
        .unwrap();
        assert_eq!(table.class_index, 1370);
        assert_eq!(table.declared_records, 2);
        assert_eq!(table.records.len(), 2);
        assert_eq!(
            table.records[0],
            ElemTableRecord {
                offset: 6,
                id: 7,
                original_id: 7,
                creation_episode: 1072,
                last_modification_episode: 2007,
                last_user_modification_episode: 2007,
                partition_id: 13,
                owning_element_id: -1,
            }
        );
        assert_eq!(table.records[1].id, 9);
        assert_eq!(table.records[1].owning_element_id, 7);
        assert_eq!(table.trailing_bytes().len(), 8, "the tail is preserved");
        assert_eq!(table.unique_id_count(), 2);
        assert_eq!(table.copied_in_count(), 1, "record 1 was copied in");
    }

    #[test]
    fn refuses_a_count_the_payload_cannot_hold() {
        // What a layout this does not understand looks like: the count reads
        // as something the array could not possibly hold. Saying so is the
        // point - the heuristic this replaced would have found a `0xff` run
        // somewhere and read a table out of the middle of the bytes.
        assert_eq!(
            ElemTable::parse(&fixture(4, &[[1, 1, 0, 0, 0, 0, -1]], 0)),
            Err(ElemTableError::RecordsDoNotFit {
                declared: 4,
                available: 1,
            })
        );
    }

    #[test]
    fn refuses_a_payload_too_short_for_the_header() {
        assert_eq!(
            ElemTable::parse(&[0x5a, 0x05, 0x01]),
            Err(ElemTableError::HeaderTooShort { actual: 3 })
        );
    }
}
