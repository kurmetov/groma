//! What a source file is, decided by its own leading bytes.
//!
//! This is the one place in the workspace that answers that question. It used
//! to be answered twice - `looks_like_step` in the CLI and a private `Format`
//! in the server's upload handler - which is how the CLI came to accept an
//! IFC for `export-scene` and refuse one for `export-json`.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

/// Leading bytes a sniff needs. Every signature below lives well inside this,
/// and a byte-order mark plus leading whitespace has to fit in front of the
/// longest of them.
pub const SNIFF_BYTES: usize = 64;

/// A source format this workspace can identify.
///
/// Identifying a format is not the same as reading one: see
/// [`Format::is_readable`]. A format is listed here as soon as it can be told
/// apart from the others, because "this is a DWG and no reader exists yet" is
/// a better answer for a caller than "this is not a file I know".
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Format {
    /// Autodesk Revit model.
    Rvt,
    /// IFC in ISO 10303-21 exchange-file (STEP) form.
    Ifc,
    /// `AutoCAD` drawing. Detected only; nothing reads one yet.
    Dwg,
}

impl Format {
    /// Every format, in a stable order, for a caller listing what is known.
    pub const ALL: &'static [Self] = &[Self::Rvt, Self::Ifc, Self::Dwg];

    /// Read the format from a file's leading bytes.
    ///
    /// - An RVT is a compound file, which always begins with the OLE
    ///   signature.
    /// - An IFC is STEP text, which begins with `ISO-10303-21`, possibly
    ///   behind whitespace or a byte-order mark.
    /// - A DWG begins with a six-byte ASCII version tag, `AC10..` for every
    ///   release since R13. Taken from the published DWG specification and
    ///   **not** verified against a corpus here, because this repository has
    ///   no DWG fixtures; it is used to name the format in an error, never to
    ///   decide how to parse one.
    #[must_use]
    pub fn sniff(head: &[u8]) -> Option<Self> {
        const OLE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        if head.starts_with(&OLE) {
            return Some(Self::Rvt);
        }
        if head.starts_with(b"AC10") || head.starts_with(b"AC1.") || head.starts_with(b"AC2.") {
            return Some(Self::Dwg);
        }
        let text = head.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(head);
        let text = String::from_utf8_lossy(&text[..text.len().min(SNIFF_BYTES)]);
        text.trim_start()
            .starts_with("ISO-10303-21")
            .then_some(Self::Ifc)
    }

    /// Read the format from the file at `path`.
    ///
    /// `Ok(None)` means the file was read and its bytes match nothing known,
    /// which is a different answer from a read that failed.
    ///
    /// # Errors
    ///
    /// Whatever opening or reading `path` returned.
    pub fn sniff_file(path: &Path) -> io::Result<Option<Self>> {
        let mut head = [0_u8; SNIFF_BYTES];
        let mut filled = 0;
        let mut file = File::open(path)?;
        while filled < head.len() {
            match file.read(&mut head[filled..])? {
                0 => break,
                read => filled += read,
            }
        }
        Ok(Self::sniff(&head[..filled]))
    }

    /// Whether a reader for this format exists. A detected-only format is
    /// still worth naming, so this is asked separately from [`Self::sniff`].
    #[must_use]
    pub fn is_readable(self) -> bool {
        match self {
            Self::Rvt | Self::Ifc => true,
            Self::Dwg => false,
        }
    }

    /// The extension a file of this format is stored under.
    #[must_use]
    pub fn extension(self) -> &'static str {
        self.extensions()[0]
    }

    /// Every extension that names this format, lower-case, the canonical one
    /// first. Used to strip a format's suffix off a name and to recognise one
    /// where no bytes are at hand.
    #[must_use]
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rvt => &["rvt", "rfa"],
            Self::Ifc => &["ifc", "ifczip", "step", "stp"],
            Self::Dwg => &["dwg"],
        }
    }

    /// What to call this format to a person.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Rvt => "Revit model",
            Self::Ifc => "IFC file",
            Self::Dwg => "AutoCAD drawing",
        }
    }

    /// The short tag a scene records as the kind of source it was built from.
    /// It is written into a scene file, so it must not change.
    #[must_use]
    pub fn source_kind(self) -> &'static str {
        match self {
            Self::Rvt => "rvt",
            Self::Ifc => "ifc",
            Self::Dwg => "dwg",
        }
    }

    /// How much memory this format's reader needs, as a multiple of the
    /// source's own size, or `None` where the reader is not whole-file and
    /// its cost is bounded some other way.
    ///
    /// An RVT is read a compressed member at a time and bounded by its own
    /// `--max-member-bytes`, so its cost is not a multiple of the file. A
    /// whole-file text reader's is, and it is the geometry rather than the
    /// text that decides it: the reference model's own 643 MB IFC export
    /// peaks at 2.20 GB - 3.4x - while a 1.8 GiB structural model of 16.6
    /// million instances and 23.8 million declared edges reaches 9.0 GiB
    /// through the same `export-scene`, which is 5.0x. Five is the worse of
    /// the two, because a ceiling set from the lighter file is a ceiling that
    /// invites the machine to swap on the heavier one.
    ///
    /// Writing an IFC back out costs more again - the entity graph of the
    /// file being written is held alongside the model it is written from, and
    /// that same structural model peaks at 22.8 GiB through `export-ifc` -
    /// but that is the exporter's cost and the same for either source format,
    /// so it is not what this ratio measures.
    #[must_use]
    // The two `None` arms are not the same answer: one format's cost is
    // bounded elsewhere, the other has never been measured. Merging them
    // would lose exactly the distinction rule 12 asks to keep.
    #[allow(clippy::match_same_arms)]
    pub fn memory_ratio(self) -> Option<u64> {
        match self {
            Self::Rvt => None,
            Self::Ifc => Some(5),
            // No reader, so no measurement. Deliberately absent rather than
            // guessed from the IFC figure.
            Self::Dwg => None,
        }
    }

    /// The format an extension names, where no bytes are available. Bytes are
    /// the better answer and [`Self::sniff`] should be preferred.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let lowered = extension.trim_start_matches('.').to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.extensions().contains(&lowered.as_str()))
    }

    /// `name` with a recognised format extension removed, whatever its case.
    #[must_use]
    pub fn strip_extension(name: &str) -> &str {
        let Some((stem, extension)) = name.rsplit_once('.') else {
            return name;
        };
        if Self::from_extension(extension).is_some() {
            stem
        } else {
            name
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ole_signature_is_a_revit_model() {
        let head = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 0x00, 0x00];
        assert_eq!(Format::sniff(&head), Some(Format::Rvt));
    }

    #[test]
    fn a_step_header_is_an_ifc_behind_a_byte_order_mark_and_whitespace() {
        let mut head = vec![0xef, 0xbb, 0xbf];
        head.extend_from_slice(b"\r\n  ISO-10303-21;\nHEADER;");
        assert_eq!(Format::sniff(&head), Some(Format::Ifc));
    }

    #[test]
    fn a_drawing_is_named_but_not_readable() {
        assert_eq!(Format::sniff(b"AC1032\x00\x00\x00"), Some(Format::Dwg));
        assert!(!Format::Dwg.is_readable());
        assert!(Format::Rvt.is_readable());
    }

    #[test]
    fn unknown_bytes_are_no_format_rather_than_a_guess() {
        assert_eq!(Format::sniff(b"not a model at all"), None);
        assert_eq!(Format::sniff(&[]), None);
    }

    #[test]
    fn an_extension_is_recognised_in_any_case_and_stripped_once() {
        assert_eq!(Format::from_extension(".RVT"), Some(Format::Rvt));
        assert_eq!(Format::from_extension("ifc"), Some(Format::Ifc));
        assert_eq!(Format::from_extension("txt"), None);
        assert_eq!(Format::strip_extension("Tower.A1.IFC"), "Tower.A1");
        assert_eq!(Format::strip_extension("Tower.A1"), "Tower.A1");
    }

    #[test]
    fn only_a_whole_file_reader_declares_a_memory_ratio() {
        assert_eq!(Format::Ifc.memory_ratio(), Some(5));
        assert_eq!(Format::Rvt.memory_ratio(), None);
    }
}
