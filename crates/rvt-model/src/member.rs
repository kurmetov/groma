use std::fmt;

/// Body bytes a continuation shifts from the receiving member to the member
/// that handed the record on. Measured, not explained.
const CONTINUATION_BODY_CORRECTION: u64 = 4;

/// Header width and length-field position for one member record layout.
///
/// Both layouts were recovered by requiring an exact tiling of the decoded
/// member: the walk must consume every byte and produce exactly the record
/// count declared by the partition-member descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordLayout {
    /// 12-byte header; the body length is the `u32` at `+4`.
    Narrow,
    /// 16-byte header; the body length is the `u32` at `+8`.
    Wide,
}

impl RecordLayout {
    /// Layout selected by the descriptor's format tag, when it is a known one.
    #[must_use]
    pub const fn from_format_tag(tag: u32) -> Option<Self> {
        match tag {
            101 => Some(Self::Narrow),
            102 | 103 => Some(Self::Wide),
            _ => None,
        }
    }

    #[must_use]
    pub const fn header_bytes(self) -> usize {
        match self {
            Self::Narrow => 12,
            Self::Wide => 16,
        }
    }

    #[must_use]
    pub const fn length_offset(self) -> usize {
        match self {
            Self::Narrow => 4,
            Self::Wide => 8,
        }
    }
}

/// One record inside a decoded member, kept as offsets into the payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemberRecord {
    /// Offset of the record header inside the decoded member.
    pub offset: usize,
    pub header_bytes: usize,
    pub body_bytes: usize,
}

impl MemberRecord {
    #[must_use]
    pub const fn body_offset(&self) -> usize {
        self.offset + self.header_bytes
    }

    #[must_use]
    pub const fn end(&self) -> usize {
        self.offset + self.header_bytes + self.body_bytes
    }

    /// The record's body bytes within `payload`, or the empty slice when the
    /// record is not wholly present.
    ///
    /// A record may be continued into the following member, so a declared
    /// `end()` past the end of one member's payload is an expected condition
    /// rather than corruption. Reading the body through this accessor keeps
    /// that case from becoming a panic.
    #[must_use]
    pub fn body_in<'a>(&self, payload: &'a [u8]) -> &'a [u8] {
        payload
            .get(self.body_offset()..self.end())
            .unwrap_or_default()
    }
}

/// A record walk that allows a record to continue into the next member.
///
/// The strict [`MemberRecords::parse`] is the special case where nothing is
/// carried in and nothing is left owing at the end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberWalk {
    pub layout: RecordLayout,
    /// Payload bytes belonging to a record continued from the previous member.
    pub leading_carry: usize,
    /// Records whose header starts inside this member.
    pub records: Vec<MemberRecord>,
    /// Declared body bytes of the records starting in this member.
    pub body_bytes: u64,
    /// Body bytes physically stored in this member, including a carried tail
    /// and excluding the part of a continued record left for the next member.
    pub body_bytes_in_member: u64,
    /// Bytes the final record still expects from the next member.
    pub trailing_deficit: u64,
}

impl MemberWalk {
    /// Walk a decoded member, treating the first `leading_carry` bytes as the
    /// tail of a record that started in an earlier member.
    ///
    /// A record whose body runs past the payload is reported through
    /// [`MemberWalk::trailing_deficit`] instead of failing, so a caller can
    /// verify continuation by checking that the next member carries exactly
    /// that many bytes. Everything else stays strict: a header must fit, and
    /// the walk must land on the payload end.
    ///
    /// # Errors
    ///
    /// Returns an error when a record header is cut by the payload end or when
    /// a declared length overflows the address space.
    pub fn parse(
        payload: &[u8],
        layout: RecordLayout,
        leading_carry: usize,
    ) -> Result<Self, MemberRecordError> {
        let header_bytes = layout.header_bytes();
        let length_offset = layout.length_offset();
        let mut records = Vec::new();
        let mut body_bytes = 0_u64;

        if leading_carry >= payload.len() {
            return Ok(Self {
                layout,
                leading_carry,
                records,
                body_bytes,
                body_bytes_in_member: payload.len() as u64,
                trailing_deficit: (leading_carry - payload.len()) as u64,
            });
        }

        let mut offset = leading_carry;
        let mut trailing_deficit = 0_u64;
        while offset < payload.len() {
            let Some(header) = payload.get(offset..offset + header_bytes) else {
                return Err(MemberRecordError::TruncatedHeader { offset });
            };
            let length = u32::from_le_bytes([
                header[length_offset],
                header[length_offset + 1],
                header[length_offset + 2],
                header[length_offset + 3],
            ]);
            let body = usize::try_from(length).map_err(|_| MemberRecordError::BodyOverflow {
                offset,
                length: u64::from(length),
            })?;
            let end = offset
                .checked_add(header_bytes)
                .and_then(|start| start.checked_add(body))
                .ok_or(MemberRecordError::BodyOverflow {
                    offset,
                    length: u64::from(length),
                })?;
            records.push(MemberRecord {
                offset,
                header_bytes,
                body_bytes: body,
            });
            body_bytes += u64::from(length);
            if end > payload.len() {
                trailing_deficit = (end - payload.len()) as u64;
                break;
            }
            offset = end;
        }

        let header_total = (records.len() * header_bytes) as u64;
        Ok(Self {
            layout,
            leading_carry,
            records,
            body_bytes,
            body_bytes_in_member: payload.len() as u64 - header_total,
            trailing_deficit,
        })
    }

    /// Records that both start and end inside this member.
    #[must_use]
    pub fn complete_record_count(&self) -> usize {
        self.records.len() - usize::from(self.trailing_deficit > 0)
    }

    /// Body-byte total the descriptor's `+28` field carries for this walk.
    ///
    /// A member that hands the tail of a record to the next member counts four
    /// more body bytes than it physically stores, and the member that receives
    /// that tail counts four fewer. The rule was measured on every continuation
    /// in the corpus, including members that both receive a tail and hand one
    /// on, where the two corrections cancel. Its cause is not established.
    #[must_use]
    pub fn descriptor_body_bytes(&self) -> u64 {
        let handed_on = u64::from(self.trailing_deficit > 0) * CONTINUATION_BODY_CORRECTION;
        let received = u64::from(self.leading_carry > 0) * CONTINUATION_BODY_CORRECTION;
        self.body_bytes_in_member
            .saturating_add(handed_on)
            .saturating_sub(received)
    }

    /// Whether the descriptor's record count and body total both match this
    /// walk, applying the continuation correction.
    #[must_use]
    pub fn matches_descriptor(&self, record_count: u32, record_body_bytes: u32) -> bool {
        self.records.len() as u64 == u64::from(record_count)
            && self.descriptor_body_bytes() == u64::from(record_body_bytes)
    }
}

/// Fields recovered from one record header.
///
/// The identifier and class index are corpus-verified; the remaining words are
/// exposed under neutral names because their meaning is not established.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordHeader {
    /// `+0`. Matches a `Global/ElemTable` candidate ID for most records.
    pub id: u32,
    /// Declared body length, from `+4` (narrow) or `+8` (wide).
    pub body_bytes: u32,
    /// Low half of the trailing word: an index into the decoded schema.
    pub class_index: u16,
    /// High half of the trailing word. Small enumerated value, meaning unknown.
    pub companion: u16,
    /// `+4` of a wide header, which the narrow layout does not have.
    pub wide_word: Option<u32>,
}

impl RecordHeader {
    /// Read the header of `record` out of its member payload.
    #[must_use]
    pub fn parse(payload: &[u8], record: &MemberRecord, layout: RecordLayout) -> Option<Self> {
        let header = payload.get(record.offset..record.offset + record.header_bytes)?;
        let read_u32 = |at: usize| {
            u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        let trailing = match layout {
            RecordLayout::Narrow => 8,
            RecordLayout::Wide => 12,
        };
        Some(Self {
            id: read_u32(0),
            body_bytes: read_u32(layout.length_offset()),
            class_index: u16::from_le_bytes([header[trailing], header[trailing + 1]]),
            companion: u16::from_le_bytes([header[trailing + 2], header[trailing + 3]]),
            wide_word: match layout {
                RecordLayout::Narrow => None,
                RecordLayout::Wide => Some(read_u32(4)),
            },
        })
    }
}

/// Record array recovered from one decoded partition member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberRecords {
    pub layout: RecordLayout,
    pub records: Vec<MemberRecord>,
    /// Total body bytes; comparable with the descriptor's `record_body_bytes`.
    pub body_bytes: u64,
}

impl MemberRecords {
    /// Walk a decoded member as a sequence of length-prefixed records.
    ///
    /// The walk is strict: every record must fit, and the last record must end
    /// exactly at the end of the payload. Nothing is resynchronized or skipped,
    /// so a member that does not tile is reported as an error rather than as a
    /// partial record list.
    ///
    /// # Errors
    ///
    /// Returns an error when a header or body would run past the payload, when
    /// a record is empty, or when the walk does not land on the payload end.
    pub fn parse(payload: &[u8], layout: RecordLayout) -> Result<Self, MemberRecordError> {
        let header_bytes = layout.header_bytes();
        let length_offset = layout.length_offset();
        let mut records = Vec::new();
        let mut body_bytes = 0_u64;
        let mut offset = 0_usize;

        while offset < payload.len() {
            let Some(header) = payload.get(offset..offset + header_bytes) else {
                return Err(MemberRecordError::TruncatedHeader { offset });
            };
            let length = u32::from_le_bytes([
                header[length_offset],
                header[length_offset + 1],
                header[length_offset + 2],
                header[length_offset + 3],
            ]);
            let body = usize::try_from(length).map_err(|_| MemberRecordError::BodyOverflow {
                offset,
                length: u64::from(length),
            })?;
            let end = offset
                .checked_add(header_bytes)
                .and_then(|start| start.checked_add(body))
                .ok_or(MemberRecordError::BodyOverflow {
                    offset,
                    length: u64::from(length),
                })?;
            if end > payload.len() {
                return Err(MemberRecordError::BodyPastEnd {
                    offset,
                    length: u64::from(length),
                    remaining: payload.len() - offset,
                });
            }
            records.push(MemberRecord {
                offset,
                header_bytes,
                body_bytes: body,
            });
            body_bytes += u64::from(length);
            offset = end;
        }

        if offset != payload.len() {
            return Err(MemberRecordError::TrailingBytes {
                offset,
                total: payload.len(),
            });
        }
        Ok(Self {
            layout,
            records,
            body_bytes,
        })
    }

    /// Whether the walk reproduced the descriptor's declared record count.
    #[must_use]
    pub fn matches_declared_count(&self, declared: u32) -> bool {
        u64::try_from(self.records.len()).is_ok_and(|count| count == u64::from(declared))
    }

    /// Whether the walk reproduced the descriptor's declared body-byte total.
    #[must_use]
    pub fn matches_declared_body_bytes(&self, declared: u32) -> bool {
        self.body_bytes == u64::from(declared)
    }

    #[must_use]
    pub fn record_bytes<'payload>(
        &self,
        payload: &'payload [u8],
        record: &MemberRecord,
    ) -> Option<&'payload [u8]> {
        payload.get(record.offset..record.end())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberRecordError {
    TruncatedHeader {
        offset: usize,
    },
    BodyPastEnd {
        offset: usize,
        length: u64,
        remaining: usize,
    },
    BodyOverflow {
        offset: usize,
        length: u64,
    },
    TrailingBytes {
        offset: usize,
        total: usize,
    },
}

impl fmt::Display for MemberRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader { offset } => {
                write!(formatter, "record header at {offset} is truncated")
            }
            Self::BodyPastEnd {
                offset,
                length,
                remaining,
            } => write!(
                formatter,
                "record at {offset} declares {length} body bytes but only {remaining} remain"
            ),
            Self::BodyOverflow { offset, length } => write!(
                formatter,
                "record at {offset} declares {length} body bytes, which overflows the payload span"
            ),
            Self::TrailingBytes { offset, total } => write!(
                formatter,
                "record walk stopped at {offset} of {total} decoded bytes"
            ),
        }
    }
}

impl std::error::Error for MemberRecordError {}

/// Fields recovered from the body of an `ElementHeader` record.
///
/// The schema declares `ElementHeader` as a leading word followed by six
/// `ElementId` properties. Two of them are corpus-verified: the category code
/// at `+2` and the family reference at `+6`. The rest are exposed as raw
/// values because their alignment beyond `+10` is not established.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElementHeaderFields {
    /// `+0`. Observed as zero throughout the corpus.
    pub leading_word: u16,
    /// `+2`. Revit category code, negative by construction. `None` when null.
    pub category: Option<i32>,
    /// `+6`. Reference to the owning family element. `None` when null.
    pub family_id: Option<i32>,
    /// `+10`, `+14`, `+18`, `+22`. Declared as `ElementId` by the schema; the
    /// values are preserved without claiming their alignment is confirmed.
    pub unverified_ids: [i32; 4],
}

/// Bytes an `ElementHeader` body needs before its identifier block is complete.
pub const ELEMENT_HEADER_ID_BLOCK_BYTES: usize = 26;
/// Value an unset `ElementId` field carries.
const NULL_ELEMENT_ID: i32 = -1;

impl ElementHeaderFields {
    /// Read the identifier block at the start of an `ElementHeader` body.
    ///
    /// Returns `None` when the body is shorter than the block; no field is
    /// inferred from a truncated record.
    #[must_use]
    pub fn parse(body: &[u8]) -> Option<Self> {
        let block = body.get(..ELEMENT_HEADER_ID_BLOCK_BYTES)?;
        let read_i32 = |at: usize| {
            i32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
        };
        let optional = |value: i32| (value != NULL_ELEMENT_ID).then_some(value);
        Some(Self {
            leading_word: u16::from_le_bytes([block[0], block[1]]),
            category: optional(read_i32(2)),
            family_id: optional(read_i32(6)),
            unverified_ids: [read_i32(10), read_i32(14), read_i32(18), read_i32(22)],
        })
    }
}

/// Fields of the shared `Element` base class, read after the `m_id` anchor.
///
/// The bytes before `m_id` are a variable-length block of object pointers that
/// is not decoded yet, so the reader anchors on the identifier the record
/// header already provides and reads only the fixed tail the schema declares:
/// seven `ElementId` slots followed by three `Bool` flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElementFields {
    /// Offset of the `m_id` anchor inside the body.
    pub id_offset: usize,
    /// How that offset was reached.
    pub anchor: ElementAnchor,
    /// Level the element is associated with; `None` when unset.
    pub assoc_level_id: Option<i32>,
    pub family_id: Option<i32>,
    pub unplaced_owner_id: Option<i32>,
    pub owner_view_id: Option<i32>,
    pub created_phase_id: Option<i32>,
    pub demolished_phase_id: Option<i32>,
    pub design_option_id: Option<i32>,
    pub locked: bool,
    pub moribund: bool,
    pub dummy: bool,
}

/// Bytes the `Element` tail occupies: seven `ElementId` slots and three flags.
pub const ELEMENT_TAIL_BYTES: usize = 7 * 4 + 3;
/// How the `m_id` anchor inside a body was located.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElementAnchor {
    /// Reached by walking the leading pointer block, with no search involved.
    PointerBlock,
    /// Found by searching the start of the body for the record's identifier.
    IdentifierSearch,
}

/// Bytes between the end of the pointer block and `m_id`.
///
/// Observed as `01 00 00 00` throughout the corpus; the schema places
/// `m_docAccess` there, but its encoding is not established.
pub const ELEMENT_PRE_ID_WORD_BYTES: usize = 4;

/// Window at the start of a body in which the `m_id` anchor is searched.
///
/// The anchor was observed between `+34` and `+42`; a body can repeat its own
/// identifier later on, so the search is bounded instead of scanning to the
/// end and rejecting every element that references itself.
pub const ELEMENT_ID_SEARCH_WINDOW_BYTES: usize = 128;

impl ElementFields {
    /// Locate `m_id` in a body and read the `Element` tail that follows it.
    ///
    /// Returns `None` when the identifier is absent, when it appears more than
    /// once (the anchor would be ambiguous), or when the tail does not fit.
    #[must_use]
    pub fn parse(body: &[u8], record_id: u32) -> Option<Self> {
        Self::parse_from_pointer_block(body, record_id)
            .or_else(|| Self::parse_by_searching(body, record_id))
    }

    /// Walk the leading block of object pointers to reach `m_id` directly.
    ///
    /// A null pointer is `00 00`; a reference is `ff ff ff ff` followed by a
    /// `u16` class index. The block is followed by one four-byte word and then
    /// `m_id`, which must equal the identifier the record header declares —
    /// the walk is accepted only when that check passes.
    #[must_use]
    pub fn parse_from_pointer_block(body: &[u8], record_id: u32) -> Option<Self> {
        let mut offset = 0_usize;
        loop {
            match body.get(offset..offset + 2) {
                Some([0, 0]) => offset += 2,
                Some([0xff, 0xff]) if body.get(offset..offset + 4)? == [0xff; 4] => {
                    offset = offset.checked_add(6)?;
                }
                _ => break,
            }
        }
        let id_offset = offset.checked_add(ELEMENT_PRE_ID_WORD_BYTES)?;
        let declared = body.get(id_offset..id_offset + 4)?;
        if declared != record_id.to_le_bytes() {
            return None;
        }
        Self::read_tail(body, id_offset, ElementAnchor::PointerBlock)
    }

    /// Fall back to searching the start of the body for the identifier.
    #[must_use]
    pub fn parse_by_searching(body: &[u8], record_id: u32) -> Option<Self> {
        let wanted = record_id.to_le_bytes();
        let window = body.len().min(ELEMENT_ID_SEARCH_WINDOW_BYTES);
        let mut anchor = None;
        for offset in 0..window.saturating_sub(3) {
            if body[offset..offset + 4] == wanted {
                if anchor.is_some() {
                    return None;
                }
                anchor = Some(offset);
            }
        }
        Self::read_tail(body, anchor?, ElementAnchor::IdentifierSearch)
    }

    fn read_tail(body: &[u8], id_offset: usize, anchor: ElementAnchor) -> Option<Self> {
        let tail = body.get(id_offset + 4..id_offset + 4 + ELEMENT_TAIL_BYTES)?;
        // The three trailing flags are booleans; anything else means the
        // anchor landed on a coincidental copy of the identifier.
        if tail[28..31].iter().any(|flag| *flag > 1) {
            return None;
        }
        let slot = |index: usize| {
            let at = index * 4;
            let value = i32::from_le_bytes([tail[at], tail[at + 1], tail[at + 2], tail[at + 3]]);
            (value != NULL_ELEMENT_ID).then_some(value)
        };
        Some(Self {
            id_offset,
            anchor,
            assoc_level_id: slot(0),
            family_id: slot(1),
            unplaced_owner_id: slot(2),
            owner_view_id: slot(3),
            created_phase_id: slot(4),
            demolished_phase_id: slot(5),
            design_option_id: slot(6),
            locked: tail[28] != 0,
            moribund: tail[29] != 0,
            dummy: tail[30] != 0,
        })
    }
}

/// Serialized bytes from a dynamic `Plane` class marker through its `m_yVec`.
///
/// The layout is the two-byte dynamic class index, the `Surface` base fields
/// (`Envelope`, then `m_orientFlag`), and the three `Plane` vectors
/// (`m_origin`, `m_xVec`, `m_yVec`).
pub const LEVEL_SERIALIZED_PLANE_BYTES: usize = 2 + 4 * 8 + 1 + 3 * 3 * 8;

/// Fields recovered from the plane stored by a `Level`/`DatumPlane`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelFields {
    /// Offset of the schema-resolved dynamic `Plane` class marker.
    pub plane_offset: usize,
    /// `Plane.m_origin[2]`, in Revit's internal length unit (feet).
    pub elevation_feet: f64,
}

impl LevelFields {
    /// Locate the serialized `DatumPlane.m_pSurface` and read its origin Z.
    ///
    /// `plane_class_index` comes from `Formats/Latest`; it must not be
    /// hardcoded because schema indexes can drift by release. A candidate is
    /// accepted only when the complete plane fits, all numbers are finite,
    /// the orientation flag is boolean, and its X/Y axes are orthonormal.
    /// Multiple surviving candidates are rejected as ambiguous.
    #[must_use]
    pub fn parse(body: &[u8], plane_class_index: u16) -> Option<Self> {
        const VECTOR_TOLERANCE: f64 = 1.0e-8;

        let marker = plane_class_index.to_le_bytes();
        let mut found = None;
        for offset in 0..=body.len().checked_sub(LEVEL_SERIALIZED_PLANE_BYTES)? {
            if body.get(offset..offset + 2)? != marker {
                continue;
            }
            let plane = body.get(offset..offset + LEVEL_SERIALIZED_PLANE_BYTES)?;
            if plane[34] > 1 {
                continue;
            }
            let read_f64 = |at: usize| {
                let bytes: [u8; 8] = plane.get(at..at + 8)?.try_into().ok()?;
                Some(f64::from_le_bytes(bytes))
            };
            let envelope = [read_f64(2)?, read_f64(10)?, read_f64(18)?, read_f64(26)?];
            let origin = [read_f64(35)?, read_f64(43)?, read_f64(51)?];
            let x_axis = [read_f64(59)?, read_f64(67)?, read_f64(75)?];
            let y_axis = [read_f64(83)?, read_f64(91)?, read_f64(99)?];
            if envelope
                .into_iter()
                .chain(origin)
                .chain(x_axis)
                .chain(y_axis)
                .any(|value| !value.is_finite())
            {
                continue;
            }
            let squared_norm = |axis: [f64; 3]| axis.into_iter().map(|v| v * v).sum::<f64>();
            let dot = x_axis
                .into_iter()
                .zip(y_axis)
                .map(|(left, right)| left * right)
                .sum::<f64>();
            if (squared_norm(x_axis) - 1.0).abs() > VECTOR_TOLERANCE
                || (squared_norm(y_axis) - 1.0).abs() > VECTOR_TOLERANCE
                || dot.abs() > VECTOR_TOLERANCE
            {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(Self {
                plane_offset: offset,
                elevation_feet: origin[2],
            });
        }
        found
    }
}

/// Longest string accepted from a record body, in UTF-16 code units.
pub const MAX_STRING_CHARS: u32 = 4096;
/// Shortest string accepted. A single character is indistinguishable from
/// two arbitrary bytes, so one-character matches are rejected.
pub const MIN_STRING_CHARS: u32 = 2;

/// A length-prefixed UTF-16LE string stored inside a record body.
///
/// The encoding is `[count:u32][UTF-16LE code units]`, the same shape already
/// verified in `BasicFileInfo` and `Global/PartitionTable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordString {
    pub offset: usize,
    pub value: String,
    /// Bytes the string occupies, prefix included.
    pub encoded_bytes: usize,
}

impl RecordString {
    /// Read a string at an exact offset, requiring it to look like a name.
    ///
    /// Use this where the position is a guess and a chance decoding has to be
    /// rejected. Where the position is already established — a parameter value
    /// behind its identifier, for instance — use
    /// [`RecordString::parse_at_lenient`], since a stored value may be a
    /// single character or contain characters a name never would.
    #[must_use]
    pub fn parse_at(body: &[u8], offset: usize) -> Option<Self> {
        let parsed = Self::parse_at_lenient(body, offset)?;
        if u32::try_from(parsed.value.chars().count()).ok()? < MIN_STRING_CHARS
            || !is_plausible_name(&parsed.value)
        {
            return None;
        }
        Some(parsed)
    }

    /// Read a string at an exact offset, accepting any printable content.
    #[must_use]
    pub fn parse_at_lenient(body: &[u8], offset: usize) -> Option<Self> {
        let prefix = body.get(offset..offset + 4)?;
        let count = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
        if count == 0 || count > MAX_STRING_CHARS {
            return None;
        }
        let bytes = usize::try_from(count).ok()?.checked_mul(2)?;
        let start = offset.checked_add(4)?;
        let raw = body.get(start..start.checked_add(bytes)?)?;
        let units = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        let value = String::from_utf16(&units).ok()?;
        if value.chars().any(char::is_control) {
            return None;
        }
        Some(Self {
            offset,
            value,
            encoded_bytes: 4 + bytes,
        })
    }

    /// Scan forward for the first readable string at or after `from`.
    ///
    /// This is a search, not a layout rule: use it to calibrate where a class
    /// keeps its string, then read that offset with [`RecordString::parse_at`].
    #[must_use]
    pub fn scan_from(body: &[u8], from: usize) -> Option<Self> {
        (from..body.len().saturating_sub(4)).find_map(|offset| Self::parse_at(body, offset))
    }
}

/// Whether a decoded string looks like a stored name rather than a chance
/// decoding of arbitrary bytes.
///
/// Two rules, both script-neutral: every character must be a letter, a digit,
/// or punctuation a name can contain; and all non-ASCII characters must come
/// from a single 256-code-point block. Random bytes scatter across blocks,
/// while a real name stays inside one script.
fn is_plausible_name(value: &str) -> bool {
    let mut block: Option<u32> = None;
    for character in value.chars() {
        if !is_name_character(character) {
            return false;
        }
        let code = character as u32;
        if code < 0x80 {
            continue;
        }
        let current = code >> 8;
        match block {
            Some(seen) if seen != current => return false,
            Some(_) => {}
            None => block = Some(current),
        }
    }
    true
}

/// Characters accepted inside a stored name.
fn is_name_character(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            ' ' | '_'
                | '-'
                | '.'
                | '/'
                | '\\'
                | ','
                | '('
                | ')'
                | '['
                | ']'
                | '#'
                | ':'
                | '+'
                | '%'
                | '"'
                | '\''
                | '№'
                | '°'
                | '&'
                | '*'
                | '='
                | '<'
                | '>'
                | '@'
                | '!'
                | '?'
                | ';'
                | '~'
                | '^'
                | '|'
                | '{'
                | '}'
                | '$'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(layout: RecordLayout, bodies: &[usize]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, body) in bodies.iter().enumerate() {
            let mut header = vec![0_u8; layout.header_bytes()];
            header[..4].copy_from_slice(&u32::try_from(index + 1).unwrap().to_le_bytes());
            let length = u32::try_from(*body).unwrap();
            let at = layout.length_offset();
            header[at..at + 4].copy_from_slice(&length.to_le_bytes());
            bytes.extend(header);
            bytes.extend(vec![0x5a; *body]);
        }
        bytes
    }

    #[test]
    fn walks_wide_records_to_an_exact_tiling() {
        let bytes = payload(RecordLayout::Wide, &[8, 0, 40]);
        let parsed = MemberRecords::parse(&bytes, RecordLayout::Wide).unwrap();

        assert_eq!(parsed.records.len(), 3);
        assert_eq!(parsed.records[0].offset, 0);
        assert_eq!(parsed.records[1].offset, 24);
        assert_eq!(parsed.records[2].end(), bytes.len());
        assert_eq!(parsed.body_bytes, 48);
        assert!(parsed.matches_declared_count(3));
        assert!(parsed.matches_declared_body_bytes(48));
    }

    #[test]
    fn walks_narrow_records_with_the_twelve_byte_header() {
        let bytes = payload(RecordLayout::Narrow, &[4, 16]);
        let parsed = MemberRecords::parse(&bytes, RecordLayout::Narrow).unwrap();

        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[1].offset, 16);
        assert_eq!(
            parsed
                .record_bytes(&bytes, &parsed.records[1])
                .unwrap()
                .len(),
            28
        );
        assert!(!parsed.matches_declared_count(3));
    }

    #[test]
    fn reads_the_identifier_and_class_index_from_a_header() {
        let mut bytes = payload(RecordLayout::Wide, &[8]);
        bytes[4..8].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
        bytes[12..14].copy_from_slice(&1856_u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&2_u16.to_le_bytes());
        let parsed = MemberRecords::parse(&bytes, RecordLayout::Wide).unwrap();
        let header = RecordHeader::parse(&bytes, &parsed.records[0], RecordLayout::Wide).unwrap();

        assert_eq!(header.id, 1);
        assert_eq!(header.body_bytes, 8);
        assert_eq!(header.class_index, 1856);
        assert_eq!(header.companion, 2);
        assert_eq!(header.wide_word, Some(0xdead_beef));
    }

    #[test]
    fn reads_a_narrow_header_without_the_extra_word() {
        let mut bytes = payload(RecordLayout::Narrow, &[4]);
        bytes[8..10].copy_from_slice(&1398_u16.to_le_bytes());
        let parsed = MemberRecords::parse(&bytes, RecordLayout::Narrow).unwrap();
        let header = RecordHeader::parse(&bytes, &parsed.records[0], RecordLayout::Narrow).unwrap();

        assert_eq!(header.id, 1);
        assert_eq!(header.body_bytes, 4);
        assert_eq!(header.class_index, 1398);
        assert_eq!(header.wide_word, None);
    }

    #[test]
    fn reads_the_category_and_family_from_an_element_header_body() {
        let mut body = vec![0xff_u8; ELEMENT_HEADER_ID_BLOCK_BYTES + 8];
        body[..2].copy_from_slice(&0_u16.to_le_bytes());
        body[2..6].copy_from_slice(&(-2_000_011_i32).to_le_bytes());
        body[6..10].copy_from_slice(&417_563_i32.to_le_bytes());
        let fields = ElementHeaderFields::parse(&body).unwrap();

        assert_eq!(fields.leading_word, 0);
        assert_eq!(fields.category, Some(-2_000_011));
        assert_eq!(fields.family_id, Some(417_563));
        assert_eq!(fields.unverified_ids, [-1; 4]);
    }

    #[test]
    fn treats_minus_one_as_an_unset_element_id() {
        let body = vec![0xff_u8; ELEMENT_HEADER_ID_BLOCK_BYTES];
        let fields = ElementHeaderFields::parse(&body).unwrap();
        assert_eq!(fields.category, None);
        assert_eq!(fields.family_id, None);
        assert_eq!(fields.leading_word, u16::MAX);
    }

    #[test]
    fn refuses_a_body_shorter_than_the_identifier_block() {
        let body = vec![0_u8; ELEMENT_HEADER_ID_BLOCK_BYTES - 1];
        assert!(ElementHeaderFields::parse(&body).is_none());
    }

    /// Four null pointers, the pre-id word, `m_id`, and the `Element` tail.
    fn element_body(id: u32, level: i32, phase: i32) -> Vec<u8> {
        let mut body = vec![0x00_u8; 8];
        body.extend(1_u32.to_le_bytes());
        body.extend(id.to_le_bytes());
        body.extend(level.to_le_bytes());
        for _ in 0..3 {
            body.extend((-1_i32).to_le_bytes());
        }
        body.extend(phase.to_le_bytes());
        body.extend((-1_i32).to_le_bytes());
        body.extend((-4_i32).to_le_bytes());
        body.extend([0, 1, 0]);
        body
    }

    #[test]
    fn reads_the_element_tail_after_the_identifier_anchor() {
        let body = element_body(417_563, 308_373, 3);
        let fields = ElementFields::parse(&body, 417_563).unwrap();

        assert_eq!(fields.id_offset, 12);
        assert_eq!(fields.anchor, ElementAnchor::PointerBlock);
        assert_eq!(fields.owner_view_id, None);
        assert_eq!(fields.assoc_level_id, Some(308_373));
        assert_eq!(fields.family_id, None);
        assert_eq!(fields.created_phase_id, Some(3));
        assert_eq!(fields.demolished_phase_id, None);
        assert_eq!(fields.design_option_id, Some(-4));
        assert!(!fields.locked);
        assert!(fields.moribund);
        assert!(!fields.dummy);
    }

    #[test]
    fn walks_references_inside_the_pointer_block() {
        let mut body = vec![0x00_u8, 0x00];
        body.extend([0xff, 0xff, 0xff, 0xff]);
        body.extend(1856_u16.to_le_bytes());
        body.extend([0x00, 0x00]);
        body.extend(1_u32.to_le_bytes());
        let anchor_at = body.len();
        body.extend(9_u32.to_le_bytes());
        body.extend(11_i32.to_le_bytes());
        for _ in 0..5 {
            body.extend((-1_i32).to_le_bytes());
        }
        body.extend((-4_i32).to_le_bytes());
        body.extend([0, 0, 0]);

        let fields = ElementFields::parse(&body, 9).unwrap();
        assert_eq!(fields.anchor, ElementAnchor::PointerBlock);
        assert_eq!(fields.id_offset, anchor_at);
        assert_eq!(fields.assoc_level_id, Some(11));
    }

    #[test]
    fn falls_back_to_the_search_anchor_when_the_block_does_not_walk() {
        let mut body = vec![0x5a_u8; 6];
        body.extend(77_u32.to_le_bytes());
        body.extend(11_i32.to_le_bytes());
        for _ in 0..5 {
            body.extend((-1_i32).to_le_bytes());
        }
        body.extend((-4_i32).to_le_bytes());
        body.extend([0, 0, 0]);

        let fields = ElementFields::parse(&body, 77).unwrap();
        assert_eq!(fields.anchor, ElementAnchor::IdentifierSearch);
        assert_eq!(fields.assoc_level_id, Some(11));
    }

    #[test]
    fn refuses_an_ambiguous_identifier_anchor() {
        let mut body = element_body(7, 11, 3);
        body[..4].copy_from_slice(&7_u32.to_le_bytes());
        assert!(ElementFields::parse(&body, 7).is_none());
    }

    #[test]
    fn ignores_a_repeated_identifier_outside_the_search_window() {
        let mut body = element_body(417_563, 308_373, 3);
        body.resize(ELEMENT_ID_SEARCH_WINDOW_BYTES + 64, 0);
        let at = ELEMENT_ID_SEARCH_WINDOW_BYTES + 16;
        body[at..at + 4].copy_from_slice(&417_563_u32.to_le_bytes());
        let fields = ElementFields::parse(&body, 417_563).unwrap();
        assert_eq!(fields.assoc_level_id, Some(308_373));
    }

    #[test]
    fn refuses_an_anchor_whose_trailing_flags_are_not_boolean() {
        let mut body = element_body(9, 11, 3);
        let flags = body.len() - 3;
        body[flags] = 0x5a;
        assert!(ElementFields::parse(&body, 9).is_none());
    }

    #[test]
    fn refuses_a_body_whose_tail_is_cut_short() {
        let mut body = element_body(9, 11, 3);
        body.truncate(body.len() - 1);
        assert!(ElementFields::parse(&body, 9).is_none());
    }

    fn encoded_string(value: &str) -> Vec<u8> {
        let units = value.encode_utf16().collect::<Vec<_>>();
        let mut bytes = u32::try_from(units.len()).unwrap().to_le_bytes().to_vec();
        for unit in units {
            bytes.extend(unit.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn reads_a_length_prefixed_utf16_string() {
        let bytes = encoded_string("01 Этаж");
        let parsed = RecordString::parse_at(&bytes, 0).unwrap();
        assert_eq!(parsed.value, "01 Этаж");
        assert_eq!(parsed.offset, 0);
        assert_eq!(parsed.encoded_bytes, 4 + 14);
    }

    #[test]
    fn refuses_a_string_that_is_cut_short_or_empty() {
        let mut bytes = encoded_string("Level");
        bytes.truncate(bytes.len() - 2);
        assert!(RecordString::parse_at(&bytes, 0).is_none());
        assert!(RecordString::parse_at(&0_u32.to_le_bytes(), 0).is_none());
    }

    #[test]
    fn refuses_a_single_character_match() {
        let bytes = encoded_string("A");
        assert!(RecordString::parse_at(&bytes, 0).is_none());
    }

    #[test]
    fn refuses_a_string_that_mixes_unrelated_scripts() {
        let bytes = encoded_string("저砿斴");
        assert!(RecordString::parse_at(&bytes, 0).is_none());
    }

    #[test]
    fn accepts_ascii_mixed_with_one_script() {
        let bytes = encoded_string("01 Этаж");
        assert!(RecordString::parse_at(&bytes, 0).is_some());
    }

    #[test]
    fn refuses_control_characters_inside_a_string() {
        let mut bytes = encoded_string("ab");
        bytes[4..6].copy_from_slice(&7_u16.to_le_bytes());
        assert!(RecordString::parse_at(&bytes, 0).is_none());
    }

    #[test]
    fn scans_forward_to_the_first_readable_string() {
        let mut bytes = vec![0xff_u8; 6];
        let at = bytes.len();
        bytes.extend(encoded_string("Системная панель"));
        let found = RecordString::scan_from(&bytes, 0).unwrap();
        assert_eq!(found.offset, at);
        assert_eq!(found.value, "Системная панель");
    }

    fn serialized_plane(index: u16, elevation_feet: f64) -> Vec<u8> {
        let mut bytes = index.to_le_bytes().to_vec();
        for value in [-10.0_f64, -20.0, 30.0, 40.0] {
            bytes.extend(value.to_le_bytes());
        }
        bytes.push(1);
        for value in [4.0_f64, 5.0, elevation_feet, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bytes.extend(value.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn reads_level_elevation_from_the_schema_resolved_plane() {
        let mut body = vec![0xaa; 37];
        body.extend(serialized_plane(565, 10.826_771_653_543_318));
        body.extend([0xbb; 9]);

        let fields = LevelFields::parse(&body, 565).unwrap();
        assert_eq!(fields.plane_offset, 37);
        assert!((fields.elevation_feet - 10.826_771_653_543_318).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_a_plane_with_non_orthonormal_axes() {
        let mut body = serialized_plane(565, 12.0);
        body[59..67].copy_from_slice(&2.0_f64.to_le_bytes());
        assert!(LevelFields::parse(&body, 565).is_none());
    }

    #[test]
    fn rejects_ambiguous_level_planes() {
        let mut body = serialized_plane(565, 3.0);
        body.extend(serialized_plane(565, 6.0));
        assert!(LevelFields::parse(&body, 565).is_none());
    }

    #[test]
    fn selects_the_layout_from_verified_format_tags() {
        assert_eq!(
            RecordLayout::from_format_tag(101),
            Some(RecordLayout::Narrow)
        );
        assert_eq!(RecordLayout::from_format_tag(102), Some(RecordLayout::Wide));
        assert_eq!(RecordLayout::from_format_tag(103), Some(RecordLayout::Wide));
        assert_eq!(RecordLayout::from_format_tag(7), None);
    }

    #[test]
    fn reports_a_record_that_continues_into_the_next_member() {
        let mut bytes = payload(RecordLayout::Wide, &[8, 40]);
        bytes.truncate(bytes.len() - 12);
        let walk = MemberWalk::parse(&bytes, RecordLayout::Wide, 0).unwrap();

        assert_eq!(walk.records.len(), 2);
        assert_eq!(walk.trailing_deficit, 12);
        assert_eq!(walk.complete_record_count(), 1);
        assert_eq!(walk.body_bytes, 48);
        assert_eq!(walk.body_bytes_in_member, 36);
        assert_eq!(walk.descriptor_body_bytes(), 40);
        assert!(walk.matches_descriptor(2, 40));
    }

    #[test]
    fn resumes_after_the_carried_tail_of_a_previous_record() {
        let mut bytes = vec![0x5a; 12];
        bytes.extend(payload(RecordLayout::Wide, &[8]));
        let walk = MemberWalk::parse(&bytes, RecordLayout::Wide, 12).unwrap();

        assert_eq!(walk.leading_carry, 12);
        assert_eq!(walk.records.len(), 1);
        assert_eq!(walk.records[0].offset, 12);
        assert_eq!(walk.trailing_deficit, 0);
        assert_eq!(walk.body_bytes_in_member, 20);
        assert_eq!(walk.descriptor_body_bytes(), 16);
    }

    #[test]
    fn carries_a_record_across_a_whole_member() {
        let bytes = vec![0x5a; 16];
        let walk = MemberWalk::parse(&bytes, RecordLayout::Wide, 40).unwrap();
        assert!(walk.records.is_empty());
        assert_eq!(walk.trailing_deficit, 24);
        assert_eq!(walk.body_bytes_in_member, 16);
        // A member that both receives a tail and hands one on needs no net
        // correction.
        assert_eq!(walk.descriptor_body_bytes(), 16);
    }

    #[test]
    fn rejects_a_body_that_runs_past_the_payload() {
        let mut bytes = payload(RecordLayout::Wide, &[8]);
        bytes[8..12].copy_from_slice(&64_u32.to_le_bytes());
        assert_eq!(
            MemberRecords::parse(&bytes, RecordLayout::Wide),
            Err(MemberRecordError::BodyPastEnd {
                offset: 0,
                length: 64,
                remaining: 24
            })
        );
    }

    #[test]
    fn rejects_a_truncated_final_header() {
        let mut bytes = payload(RecordLayout::Wide, &[8]);
        bytes.extend([0; 4]);
        assert_eq!(
            MemberRecords::parse(&bytes, RecordLayout::Wide),
            Err(MemberRecordError::TruncatedHeader { offset: 24 })
        );
    }

    #[test]
    fn body_in_reads_a_record_wholly_inside_the_payload() {
        let record = MemberRecord {
            offset: 2,
            header_bytes: 4,
            body_bytes: 3,
        };
        let payload = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(record.body_in(&payload), &[6, 7, 8]);
    }

    #[test]
    fn body_in_yields_nothing_for_a_record_continued_past_the_payload() {
        // A record may be continued into the following member, so a declared
        // end past this payload is expected. It must read as empty, not panic.
        let record = MemberRecord {
            offset: 2,
            header_bytes: 4,
            body_bytes: 500_000,
        };
        let payload = [0u8; 10];
        assert!(record.body_in(&payload).is_empty());
        assert!(record.end() > payload.len());
    }
}
