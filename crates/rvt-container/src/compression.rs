use std::io::Read;

use flate2::read::DeflateDecoder;

use crate::{Error, Result};

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const FIXED_HEADER_LEN: usize = 10;
const FLAG_HEADER_CRC: u8 = 0x02;
const FLAG_EXTRA: u8 = 0x04;
const FLAG_NAME: u8 = 0x08;
const FLAG_COMMENT: u8 = 0x10;
const FLAG_RESERVED: u8 = 0xe0;

/// Bytes stored for a complete Revit checksum page.
pub const REVIT_STORED_PAGE_BYTES: usize = 65_249;
/// Payload bytes in a complete Revit checksum page.
pub const REVIT_PAGE_PAYLOAD_BYTES: usize = 64_896;
/// Checksum/ECC bytes following each complete page payload.
pub const REVIT_PAGE_CHECKSUM_BYTES: usize = REVIT_STORED_PAGE_BYTES - REVIT_PAGE_PAYLOAD_BYTES;

/// Default upper bound for inflated data (1 GiB).
pub const DEFAULT_DECODE_LIMIT: usize = 1024 * 1024 * 1024;

/// The framing recognized around a stream payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamFraming {
    Raw,
    TruncatedGzip { gzip_offset: usize },
}

/// A decoded payload together with any bytes preceding the gzip member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedStream {
    pub framing: StreamFraming,
    pub prefix: Vec<u8>,
    pub payload: Vec<u8>,
}

/// Remove checksum/ECC trailers from complete Revit storage pages.
///
/// The final short page is retained verbatim. Callers should only use this on
/// a stream whose unmodified bytes failed deterministic decoding; stored-byte
/// APIs deliberately never apply it automatically.
#[must_use]
pub fn strip_revit_page_checksums(input: &[u8]) -> Vec<u8> {
    let full_pages = input.len() / REVIT_STORED_PAGE_BYTES;
    if full_pages == 0 {
        return input.to_vec();
    }

    let remainder = input.len() - full_pages * REVIT_STORED_PAGE_BYTES;
    let mut output = Vec::with_capacity(full_pages * REVIT_PAGE_PAYLOAD_BYTES + remainder);
    for page in 0..full_pages {
        let start = page * REVIT_STORED_PAGE_BYTES;
        output.extend_from_slice(&input[start..start + REVIT_PAGE_PAYLOAD_BYTES]);
    }
    output.extend_from_slice(&input[full_pages * REVIT_STORED_PAGE_BYTES..]);
    output
}

/// Decode RVT framing at the offsets currently verified in public fixtures.
///
/// Unknown framing is returned byte-for-byte as [`StreamFraming::Raw`]. A
/// recognized prefix is retained separately instead of being discarded.
///
/// # Errors
///
/// Returns an error when recognized gzip framing is malformed, inflation
/// fails, or the decoded payload exceeds `limit`.
pub fn decode_known_framing(input: &[u8], limit: usize) -> Result<DecodedStream> {
    let gzip_offset = [0_usize, 4, 8]
        .into_iter()
        .find(|offset| input.get(*offset..*offset + 2) == Some(GZIP_MAGIC.as_slice()));

    let Some(offset) = gzip_offset else {
        if input.len() > limit {
            return Err(Error::DecodedStreamTooLarge { limit });
        }
        return Ok(DecodedStream {
            framing: StreamFraming::Raw,
            prefix: Vec::new(),
            payload: input.to_vec(),
        });
    };

    Ok(DecodedStream {
        framing: StreamFraming::TruncatedGzip {
            gzip_offset: offset,
        },
        prefix: input[..offset].to_vec(),
        payload: decode_truncated_gzip(&input[offset..], limit)?,
    })
}

/// Inflate a gzip member even when its CRC32/ISIZE trailer is absent.
///
/// Revit streams observed in public corpora contain a valid gzip header and a
/// raw DEFLATE body, but omit the normal eight-byte gzip trailer. Parsing the
/// header ourselves also lets us handle optional RFC 1952 header fields.
///
/// # Errors
///
/// Returns an error for a malformed/unsupported gzip header, invalid DEFLATE
/// data, an I/O failure, or decoded output larger than `limit`.
pub fn decode_truncated_gzip(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    let body_offset = gzip_body_offset(input)?;
    let decoder = DeflateDecoder::new(&input[body_offset..]);
    let mut bounded = decoder.take(limit.saturating_add(1) as u64);
    let mut output = Vec::new();
    bounded.read_to_end(&mut output)?;

    if output.len() > limit {
        return Err(Error::DecodedStreamTooLarge { limit });
    }
    Ok(output)
}

fn gzip_body_offset(input: &[u8]) -> Result<usize> {
    if input.len() < FIXED_HEADER_LEN {
        return Err(Error::InvalidGzip("header is shorter than 10 bytes"));
    }
    if input[..2] != GZIP_MAGIC {
        return Err(Error::InvalidGzip("magic bytes are missing"));
    }
    if input[2] != 8 {
        return Err(Error::InvalidGzip("compression method is not DEFLATE"));
    }

    let flags = input[3];
    if flags & FLAG_RESERVED != 0 {
        return Err(Error::InvalidGzip("reserved flag bits are set"));
    }

    let mut position = FIXED_HEADER_LEN;
    if flags & FLAG_EXTRA != 0 {
        let length_bytes = input
            .get(position..position + 2)
            .ok_or(Error::InvalidGzip("extra-field length is truncated"))?;
        let length = usize::from(u16::from_le_bytes([length_bytes[0], length_bytes[1]]));
        position = position
            .checked_add(2 + length)
            .ok_or(Error::InvalidGzip("extra-field length overflows"))?;
        if position > input.len() {
            return Err(Error::InvalidGzip("extra field is truncated"));
        }
    }
    if flags & FLAG_NAME != 0 {
        position = skip_zero_terminated(input, position, "file name is truncated")?;
    }
    if flags & FLAG_COMMENT != 0 {
        position = skip_zero_terminated(input, position, "comment is truncated")?;
    }
    if flags & FLAG_HEADER_CRC != 0 {
        position = position
            .checked_add(2)
            .ok_or(Error::InvalidGzip("header CRC position overflows"))?;
        if position > input.len() {
            return Err(Error::InvalidGzip("header CRC is truncated"));
        }
    }
    Ok(position)
}

fn skip_zero_terminated(input: &[u8], start: usize, error: &'static str) -> Result<usize> {
    let relative_end = input
        .get(start..)
        .and_then(|rest| rest.iter().position(|byte| *byte == 0))
        .ok_or(Error::InvalidGzip(error))?;
    Ok(start + relative_end + 1)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::DeflateEncoder};

    use super::*;

    fn truncated_gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).unwrap();
        let deflate = encoder.finish().unwrap();

        let mut framed = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        framed.extend(deflate);
        framed
    }

    #[test]
    fn decodes_truncated_gzip_without_trailer() {
        let input = truncated_gzip(b"rivet");
        assert_eq!(decode_truncated_gzip(&input, 100).unwrap(), b"rivet");
    }

    #[test]
    fn preserves_known_prefix() {
        let mut input = vec![1, 2, 3, 4, 5, 6, 7, 8];
        input.extend(truncated_gzip(b"payload"));
        let decoded = decode_known_framing(&input, 100).unwrap();

        assert_eq!(decoded.prefix, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(decoded.payload, b"payload");
        assert_eq!(
            decoded.framing,
            StreamFraming::TruncatedGzip { gzip_offset: 8 }
        );
    }

    #[test]
    fn returns_unknown_framing_unchanged() {
        let decoded = decode_known_framing(b"unknown", 100).unwrap();
        assert_eq!(decoded.framing, StreamFraming::Raw);
        assert_eq!(decoded.payload, b"unknown");
    }

    #[test]
    fn enforces_limit_for_unknown_framing() {
        assert!(matches!(
            decode_known_framing(b"unknown", 3),
            Err(Error::DecodedStreamTooLarge { limit: 3 })
        ));
    }

    #[test]
    fn enforces_inflated_size_limit() {
        let input = truncated_gzip(&[0; 32]);
        assert!(matches!(
            decode_truncated_gzip(&input, 16),
            Err(Error::DecodedStreamTooLarge { limit: 16 })
        ));
    }

    #[test]
    fn strips_only_complete_page_checksums() {
        let mut input = vec![0x11; REVIT_PAGE_PAYLOAD_BYTES];
        input.extend(vec![0xaa; REVIT_PAGE_CHECKSUM_BYTES]);
        input.extend(vec![0x22; REVIT_PAGE_PAYLOAD_BYTES]);
        input.extend(vec![0xbb; REVIT_PAGE_CHECKSUM_BYTES]);
        input.extend([0x33; 17]);

        let output = strip_revit_page_checksums(&input);
        assert_eq!(
            output.len(),
            2 * REVIT_PAGE_PAYLOAD_BYTES + 17,
            "both complete-page trailers should be removed"
        );
        assert!(
            output[..REVIT_PAGE_PAYLOAD_BYTES]
                .iter()
                .all(|byte| *byte == 0x11)
        );
        assert!(
            output[REVIT_PAGE_PAYLOAD_BYTES..2 * REVIT_PAGE_PAYLOAD_BYTES]
                .iter()
                .all(|byte| *byte == 0x22)
        );
        assert_eq!(&output[2 * REVIT_PAGE_PAYLOAD_BYTES..], &[0x33; 17]);
    }

    #[test]
    fn leaves_short_pages_unchanged() {
        let input = vec![0x44; REVIT_STORED_PAGE_BYTES - 1];
        assert_eq!(strip_revit_page_checksums(&input), input);
    }
}
