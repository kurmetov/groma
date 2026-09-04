use crate::{Error, Result};

/// Conservatively decoded fields from the `BasicFileInfo` stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicFileInfo {
    /// Version of the `BasicFileInfo` serialization layout, not the Revit year.
    pub format_version: u32,
    /// Revit release year, only when a known layout yields an exact value.
    pub revit_version: Option<u16>,
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

        Ok(Self {
            format_version,
            revit_version,
        })
    }
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
}
