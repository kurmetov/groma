//! `Global/History`: every episode the document has been through, which is
//! what a Revit element's own `UniqueId` is built from.
//!
//! The payload opens with `DocumentHistory`'s declared fields - `m_pADoc`,
//! `m_pIncrementTable`, `m_episodeList` (which reads as an *empty* collection
//! here; the episodes are not reached through it), `m_nextLocalSequenceNumber`,
//! `m_subsequenceNumberDeficit`, then the five inline 16-byte GUIDs
//! `m_creationGUID`, `m_detachGUID`, `m_upgradeGUID`, `m_previousUpgradeGUID`
//! and `m_saveAsGUID`. That is bytes `[0, 100)`. Six bytes follow that the
//! declarations do not explain, then a `u32` episode count at 106 and the
//! array itself: one 16-byte GUID and one byte each, 17 bytes to an entry.
//!
//! Measured on all four corpus files: the array ends exactly four bytes
//! before the payload does (`110 + 17 * count`), the trailing byte of an
//! entry is `0x28` or `0x05`, and the count agrees with the
//! `DocumentIncrement.m_totalEpisodes` of the last increment in
//! `Global/DocumentIncrementTable` - 2624 against 2624 on AR S1's later save,
//! which is also `m_greatest + 1`, so the episode ids are dense from zero.

/// Where the episode count sits, past `DocumentHistory`'s declared fields and
/// the six bytes after them that they do not explain.
const COUNT_OFFSET: usize = 106;
/// Where the array itself starts.
const ARRAY_OFFSET: usize = 110;
/// A 16-byte GUID and one byte, empirically `Episode.m_strength`.
const ENTRY_BYTES: usize = 17;
/// Bytes the array is followed by, on every file measured.
const TRAILER_BYTES: usize = 4;

/// The document's episodes, newest first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpisodeTable {
    /// Each episode's GUID in network (RFC 4122) byte order, already
    /// converted from the Microsoft mixed-endian form the file stores.
    guids: Vec<[u8; 16]>,
}

impl EpisodeTable {
    /// Parse a decoded `Global/History` payload.
    ///
    /// # Errors
    ///
    /// Returns `None` for a payload too short to hold the count, or one whose
    /// count does not leave the array ending where every measured file's does.
    /// Both are a layout this does not understand, and neither is guessed at.
    #[must_use]
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let count = payload
            .get(COUNT_OFFSET..COUNT_OFFSET + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .and_then(|count| usize::try_from(count).ok())?;
        let end = ARRAY_OFFSET.checked_add(count.checked_mul(ENTRY_BYTES)?)?;
        if end.checked_add(TRAILER_BYTES)? != payload.len() {
            return None;
        }
        let guids = (0..count)
            .map(|index| {
                let at = ARRAY_OFFSET + index * ENTRY_BYTES;
                let mut stored = [0_u8; 16];
                stored.copy_from_slice(&payload[at..at + 16]);
                to_network_order(stored)
            })
            .collect();
        Some(Self { guids })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.guids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.guids.is_empty()
    }

    /// The episode an `EpisodeId.m_id` names, in network byte order.
    ///
    /// The array is stored **newest first**, so an id counts from its end:
    /// episode 0 is the document's first and the array's last. Verified
    /// against Revit's own export on both saves of AR S1 - every element
    /// whose creation episode is in the array gets back the `GlobalId` Revit
    /// wrote for it, 11 065 of 11 738 real products on one save and 10 900 of
    /// 11 567 on the other, the remainder being elements whose creation
    /// episode the array does not carry at all.
    #[must_use]
    pub fn episode(&self, id: i32) -> Option<[u8; 16]> {
        self.guids.get(self.position_of(id)?).copied()
    }

    /// Where an `EpisodeId.m_id` sits in the array.
    #[must_use]
    pub fn position_of(&self, id: i32) -> Option<usize> {
        let id = usize::try_from(id).ok()?;
        self.guids.len().checked_sub(1)?.checked_sub(id)
    }

    /// The Revit `UniqueId` of an element created in episode `id`, in network
    /// byte order, or `None` where the array does not carry that episode.
    ///
    /// Revit builds it from the episode's GUID with the element's own id
    /// exclusive-ORed into the last four bytes, big-endian. Encoding those 16
    /// bytes as IFC's 22-character GUID is what an exporter writes as the
    /// element's `GlobalId`.
    #[must_use]
    pub fn element_unique_id(&self, episode_id: i32, element_id: u32) -> Option<[u8; 16]> {
        let mut uuid = self.episode(episode_id)?;
        let tail = u32::from_be_bytes([uuid[12], uuid[13], uuid[14], uuid[15]]) ^ element_id;
        uuid[12..16].copy_from_slice(&tail.to_be_bytes());
        Some(uuid)
    }
}

/// Microsoft's mixed-endian GUID layout to RFC 4122 network order: the first
/// four bytes reverse, the next two reverse, the next two reverse, and the
/// last eight stay as they are.
#[must_use]
fn to_network_order(stored: [u8; 16]) -> [u8; 16] {
    let mut network = stored;
    network[0..4].reverse();
    network[4..6].reverse();
    network[6..8].reverse();
    network
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(guids: &[[u8; 16]]) -> Vec<u8> {
        let mut payload = vec![0_u8; ARRAY_OFFSET];
        payload[COUNT_OFFSET..COUNT_OFFSET + 4]
            .copy_from_slice(&u32::try_from(guids.len()).unwrap().to_le_bytes());
        for guid in guids {
            payload.extend_from_slice(guid);
            payload.push(0x28);
        }
        payload.extend_from_slice(&[0; TRAILER_BYTES]);
        payload
    }

    const FIRST: [u8; 16] = [1; 16];
    const SECOND: [u8; 16] = [2; 16];
    const THIRD: [u8; 16] = [3; 16];

    #[test]
    fn an_episode_id_counts_from_the_end_of_the_array() {
        // Stored newest first, so episode 0 - the document's first - is the
        // array's last entry. Reading it as a plain index instead is what
        // made the whole forward link look unsolvable.
        let table = EpisodeTable::parse(&fixture(&[THIRD, SECOND, FIRST])).unwrap();
        assert_eq!(table.len(), 3);
        assert_eq!(table.position_of(0), Some(2));
        assert_eq!(table.position_of(2), Some(0));
        assert_eq!(table.position_of(3), None, "past the oldest episode");
        assert_eq!(table.position_of(-1), None);
        assert_eq!(table.episode(0), Some(FIRST));
        assert_eq!(table.episode(2), Some(THIRD));
    }

    #[test]
    fn converts_the_stored_guid_to_network_order() {
        let stored = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        let table = EpisodeTable::parse(&fixture(&[stored])).unwrap();
        assert_eq!(
            table.episode(0),
            Some([
                0x04, 0x03, 0x02, 0x01, 0x06, 0x05, 0x08, 0x07, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
                0x0f, 0x10,
            ])
        );
    }

    #[test]
    fn exclusive_ors_the_element_id_into_the_last_four_bytes() {
        let stored = [0; 16];
        let table = EpisodeTable::parse(&fixture(&[stored])).unwrap();
        let unique = table.element_unique_id(0, 0x1234_5678).unwrap();
        assert_eq!(&unique[12..16], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(table.element_unique_id(1, 7), None, "no such episode");
    }

    #[test]
    fn refuses_a_count_that_does_not_end_where_the_payload_does() {
        let mut payload = fixture(&[FIRST, SECOND]);
        payload.push(0);
        assert_eq!(EpisodeTable::parse(&payload), None);
        assert_eq!(EpisodeTable::parse(&[0; 8]), None);
    }
}
