use crate::{Error, Result};

/// Conservatively decoded fields from the `BasicFileInfo` stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicFileInfo {
    /// Version of the `BasicFileInfo` serialization layout, not the Revit year.
    pub format_version: u32,
    /// Revit release year, only when a known layout yields an exact value.
    pub revit_version: Option<u16>,
    /// `Unique Document GUID`, which identifies this save of the document.
    ///
    /// Read out of the labelled text block rather than off an offset, so the
    /// name is Revit's own rather than this project's reading of one. Held
    /// hyphenated and lower-case exactly as the stream states it.
    pub document_guid: Option<String>,
    /// `Unique Document Increments`: how many times the document has been
    /// saved. It orders two saves of one model, which a GUID cannot.
    pub document_increments: Option<u32>,
    /// `Worksharing`, observed as `Central` or `Local`. Part of the identity
    /// picture because a local copy and its central are different files.
    pub worksharing: Option<String>,
}

impl BasicFileInfo {
    /// Parse the stable prefix and release marker from `BasicFileInfo`.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is too short to contain its layout
    /// version. Unknown but well-formed layout versions are retained and
    /// produce `None` for the Revit release.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let version_bytes = data.get(..4).ok_or(Error::MalformedBasicFileInfo(
            "stream is shorter than four bytes",
        ))?;
        let format_version = u32::from_le_bytes([
            version_bytes[0],
            version_bytes[1],
            version_bytes[2],
            version_bytes[3],
        ]);

        let revit_version = match format_version {
            10 => parse_v10_version(data),
            13 | 14 => parse_v13_or_v14_version(data),
            _ => None,
        };

        // Every one of these comes out of the labelled block by name. A
        // layout that does not carry the block leaves them `None` rather
        // than falling back to an offset that would have to be guessed at.
        Ok(Self {
            format_version,
            revit_version,
            document_guid: labelled_field(data, "Unique Document GUID")
                .filter(|value| is_hyphenated_guid(value))
                .map(|value| value.to_ascii_lowercase()),
            document_increments: labelled_field(data, "Unique Document Increments")
                .and_then(|value| value.parse().ok()),
            worksharing: labelled_field(data, "Worksharing"),
        })
    }
}

/// The value of one `Label: value` line of `BasicFileInfo`'s labelled block.
///
/// The block is UTF-16LE text among binary fields and is *not* aligned to the
/// stream: on the measured corpus its code units sit at odd byte offsets. So
/// the label is searched for as bytes at whatever alignment it occurs, and
/// the value read from the same one.
///
/// The match has to begin a line, which is what keeps one label from being
/// answered by another: `Central model's episode GUID corresponding to the
/// last reload latest` states a GUID too, and a path could name anything.
fn labelled_field(data: &[u8], label: &str) -> Option<String> {
    let needle: Vec<u8> = format!("{label}: ")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let at = data
        .windows(needle.len())
        .enumerate()
        .find(|(start, window)| *window == needle.as_slice() && starts_a_line(data, *start))
        .map(|(start, _)| start)?;
    let value = data[at + needle.len()..]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .take_while(|&unit| unit != u16::from(b'\r') && unit != u16::from(b'\n') && unit != 0)
        .collect::<Vec<_>>();
    let value = char::decode_utf16(value)
        .collect::<core::result::Result<String, _>>()
        .ok()?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Whether a match begins a line of the block rather than sitting inside one.
///
/// The break before it is a CR/LF pair, and because the block's own alignment
/// need not be the stream's, that pair may be read either way round - which
/// is why this looks for the bytes and not for a decoded `'\n'`.
fn starts_a_line(data: &[u8], start: usize) -> bool {
    start == 0
        || data[start.saturating_sub(2)..start]
            .iter()
            .any(|&byte| byte == b'\r' || byte == b'\n')
}

/// `8-4-4-4-12` hexadecimal, which is the only form the stream states a GUID
/// in. Checked rather than trusted, so a field this does not understand is
/// dropped instead of travelling on as an identity.
fn is_hyphenated_guid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|group| group.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn parse_v10_version(data: &[u8]) -> Option<u16> {
    let value = parse_length_prefixed_utf16(data, 14)?;
    find_release_year(&value)
}

fn parse_v13_or_v14_version(data: &[u8]) -> Option<u16> {
    const VERSION_MARKER: [u8; 4] = [4, 0, 0, 0];
    let marker = data
        .windows(4)
        .position(|window| window == VERSION_MARKER)?;
    let encoded_year = data.get(marker + 4..marker + 12)?;
    let year = decode_utf16_le(encoded_year)?;
    parse_release_year(&year)
}

fn parse_length_prefixed_utf16(data: &[u8], position: usize) -> Option<String> {
    let length_bytes: [u8; 4] = data.get(position..position + 4)?.try_into().ok()?;
    let code_units = usize::try_from(u32::from_le_bytes(length_bytes)).ok()?;
    let byte_length = code_units.checked_mul(2)?;
    decode_utf16_le(data.get(position + 4..position + 4 + byte_length)?)
}

fn decode_utf16_le(data: &[u8]) -> Option<String> {
    if data.len() % 2 != 0 {
        return None;
    }
    let units = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]));
    char::decode_utf16(units)
        .collect::<std::result::Result<String, _>>()
        .ok()
}

fn find_release_year(value: &str) -> Option<u16> {
    value
        .as_bytes()
        .windows(4)
        .filter_map(|candidate| std::str::from_utf8(candidate).ok())
        .find_map(parse_release_year)
}

fn parse_release_year(value: &str) -> Option<u16> {
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year = value.parse().ok()?;
    (1997..=2200).contains(&year).then_some(year)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v14_release_marker() {
        let mut data = 14_u32.to_le_bytes().to_vec();
        data.extend([9, 9, 9]);
        data.extend([4, 0, 0, 0]);
        for unit in "2026".encode_utf16() {
            data.extend(unit.to_le_bytes());
        }

        let info = BasicFileInfo::parse(&data).unwrap();
        assert_eq!(info.format_version, 14);
        assert_eq!(info.revit_version, Some(2026));
    }

    #[test]
    fn parses_v10_release_from_first_string() {
        let value = "Autodesk Revit 2018 build";
        let mut data = 10_u32.to_le_bytes().to_vec();
        data.resize(14, 0);
        data.extend(
            u32::try_from(value.encode_utf16().count())
                .unwrap()
                .to_le_bytes(),
        );
        for unit in value.encode_utf16() {
            data.extend(unit.to_le_bytes());
        }

        let info = BasicFileInfo::parse(&data).unwrap();
        assert_eq!(info.revit_version, Some(2018));
    }

    #[test]
    fn rejects_a_truncated_stream() {
        assert!(matches!(
            BasicFileInfo::parse(&[14, 0, 0]),
            Err(Error::MalformedBasicFileInfo(_))
        ));
    }

    #[test]
    fn does_not_guess_unknown_layouts() {
        let mut data = 99_u32.to_le_bytes().to_vec();
        data.extend("2026".bytes());
        let info = BasicFileInfo::parse(&data).unwrap();
        assert_eq!(info.revit_version, None);
    }

    /// A stream carrying the labelled block the way the corpus does: UTF-16LE
    /// text whose code units are at *odd* byte offsets, because `pad` bytes
    /// of binary precede it, and whose first line is itself preceded by a
    /// break - in the file that break is what separates the block from the
    /// binary field ahead of it.
    fn with_labelled_block(pad: usize, lines: &[&str]) -> Vec<u8> {
        let mut data = 14_u32.to_le_bytes().to_vec();
        data.resize(4 + pad, 0);
        for unit in format!("\r\n{}", lines.join("\r\n")).encode_utf16() {
            data.extend(unit.to_le_bytes());
        }
        data
    }

    #[test]
    fn reads_the_document_identity_from_the_labelled_block() {
        for pad in [0, 1] {
            let data = with_labelled_block(
                pad,
                &[
                    "Worksharing: Central",
                    "Central Model Path: \\\\server\\a.rvt",
                    "Unique Document GUID: 11E41A02-892f-4b12-ba15-42a8028ff452",
                    "Unique Document Increments: 773",
                    "Model Identity: face0000-1223-3344-4455-555666666333",
                ],
            );
            let info = BasicFileInfo::parse(&data).unwrap();
            assert_eq!(
                info.document_guid.as_deref(),
                Some("11e41a02-892f-4b12-ba15-42a8028ff452"),
                "the block is found at either alignment, and the GUID lower-cased (pad {pad})"
            );
            assert_eq!(info.document_increments, Some(773));
            assert_eq!(info.worksharing.as_deref(), Some("Central"));
        }
    }

    #[test]
    fn does_not_let_another_label_answer_for_the_document_guid() {
        // This line states a GUID and ends in words that contain neither
        // label, but a substring search over the block would still have to
        // step past it; the same block without the real label must yield
        // nothing rather than this value.
        let data = with_labelled_block(
            1,
            &[
                "Central model's episode GUID corresponding to the last reload latest: \
                 11e41a02-892f-4b12-ba15-42a8028ff452",
                "Last Save Path: \\\\server\\Unique Document GUID: 0badf00d-0000-0000-0000-000000000000",
            ],
        );
        let info = BasicFileInfo::parse(&data).unwrap();
        assert_eq!(
            info.document_guid, None,
            "a label has to begin its own line to be that label"
        );
    }

    #[test]
    fn drops_a_document_guid_that_is_not_one() {
        let data = with_labelled_block(1, &["Unique Document GUID: not-a-guid"]);
        assert_eq!(BasicFileInfo::parse(&data).unwrap().document_guid, None);
    }

    #[test]
    fn states_no_identity_where_the_block_is_absent() {
        let mut data = 14_u32.to_le_bytes().to_vec();
        data.extend([4, 0, 0, 0]);
        for unit in "2023".encode_utf16() {
            data.extend(unit.to_le_bytes());
        }
        let info = BasicFileInfo::parse(&data).unwrap();
        assert_eq!(info.revit_version, Some(2023));
        assert_eq!(info.document_guid, None);
        assert_eq!(info.document_increments, None);
        assert_eq!(info.worksharing, None);
    }
}
