#![forbid(unsafe_code)]

mod basic_file_info;
mod compression;
mod error;
mod partition;

use std::{
    cell::RefCell,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

pub use basic_file_info::BasicFileInfo;
pub use compression::{
    DEFAULT_DECODE_LIMIT, DecodedStream, REVIT_PAGE_CHECKSUM_BYTES, REVIT_PAGE_PAYLOAD_BYTES,
    REVIT_STORED_PAGE_BYTES, StreamFraming, decode_known_framing, decode_truncated_gzip,
    strip_revit_page_checksums,
};
pub use error::{Error, Result};
pub use partition::{
    MARKER_ENVELOPE_MARKER_BYTES, MAX_MARKER_ENVELOPE_CONTEXT_BYTES, MEMBER_DESCRIPTOR_BYTES,
    MEMBER_STORED_SPAN_OVERHEAD, MarkerEnvelope, MarkerEnvelopeOptions, MemberDescriptor,
    PARTITION_MEMBER_PREFIX_BYTES, PartitionFailure, PartitionMember, PartitionReadOptions,
    PartitionReport,
};

const CFB_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const DEFAULT_STREAM_LIMIT: u64 = 512 * 1024 * 1024;

/// Metadata for one physical stream in the compound file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamMetadata {
    name: String,
    path: String,
    len: u64,
}

impl StreamMetadata {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Read-only access to an RVT's CFB/OLE container.
pub struct RvtContainer {
    source_path: PathBuf,
    compound: RefCell<cfb::CompoundFile<File>>,
    streams: Vec<StreamMetadata>,
}

impl RvtContainer {
    /// Open a file after validating the CFB/OLE signature.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, has the wrong signature,
    /// or its CFB directory structure cannot be parsed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let mut signature = [0_u8; 8];
        match file.read_exact(&mut signature) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Error::InvalidContainerSignature);
            }
            Err(error) => return Err(error.into()),
        }
        if signature != CFB_SIGNATURE {
            return Err(Error::InvalidContainerSignature);
        }
        file.seek(SeekFrom::Start(0))?;

        let compound = cfb::CompoundFile::open(file)?;
        let mut streams = compound
            .walk()
            .filter(cfb::Entry::is_stream)
            .map(|entry| StreamMetadata {
                name: entry.name().to_owned(),
                path: display_stream_path(entry.path()),
                len: entry.len(),
            })
            .collect::<Vec<_>>();
        streams.sort_by_key(|stream| stream.path.to_lowercase());

        Ok(Self {
            source_path: path.to_path_buf(),
            compound: RefCell::new(compound),
            streams,
        })
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    #[must_use]
    pub fn streams(&self) -> &[StreamMetadata] {
        &self.streams
    }

    #[must_use]
    pub fn stream(&self, name: &str) -> Option<&StreamMetadata> {
        let normalized = name.trim_start_matches('/');
        self.streams
            .iter()
            .find(|stream| stream.path.eq_ignore_ascii_case(normalized))
    }

    #[must_use]
    pub fn partition_count(&self) -> usize {
        self.streams
            .iter()
            .filter(|stream| {
                stream
                    .path
                    .strip_prefix("Partitions/")
                    .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            })
            .count()
    }

    /// Read a stream into memory with the default 512 MiB safety limit.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent stream, an oversized stream, or an I/O
    /// failure while reading the CFB sector chain.
    pub fn read_stream(&self, name: &str) -> Result<Vec<u8>> {
        self.read_stream_with_limit(name, DEFAULT_STREAM_LIMIT)
    }

    /// Read a stream into memory up to the caller-provided byte limit.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent stream, when its declared length exceeds
    /// `limit`, or for an I/O failure while reading it.
    pub fn read_stream_with_limit(&self, name: &str, limit: u64) -> Result<Vec<u8>> {
        let metadata = self
            .stream(name)
            .ok_or_else(|| Error::StreamNotFound(name.to_owned()))?;
        if metadata.len > limit {
            return Err(Error::StreamTooLarge {
                name: metadata.path.clone(),
                size: metadata.len,
                limit,
            });
        }

        let capacity = usize::try_from(metadata.len).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        self.copy_stream(name, &mut bytes)?;
        Ok(bytes)
    }

    /// Copy a raw stream without interpreting or buffering its contents.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or absent path, a storage path instead
    /// of a stream, or an I/O failure in either reader or writer.
    pub fn copy_stream(&self, name: &str, writer: &mut impl Write) -> Result<u64> {
        let path = checked_cfb_path(name)?;
        let mut compound = self.compound.borrow_mut();
        if !compound.exists(&path) {
            return Err(Error::StreamNotFound(name.to_owned()));
        }
        if !compound.is_stream(&path) {
            return Err(Error::NotAStream(name.to_owned()));
        }
        let mut stream = compound.open_stream(path)?;
        Ok(std::io::copy(&mut stream, writer)?)
    }

    /// Inventory independently compressed members in a `Partitions/*` stream.
    ///
    /// The stream is scanned twice without retaining decoded payload bytes:
    /// once for gzip candidates and once to validate/count each member.
    /// Checksum-page trailers are removed in-flight.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent/non-partition stream, an unsafe path,
    /// an I/O failure, or a candidate count above the configured bound.
    pub fn inspect_partition(
        &self,
        name: &str,
        options: PartitionReadOptions,
    ) -> Result<PartitionReport> {
        let metadata = self.partition_metadata(name)?;
        let path = metadata.path.clone();
        let stored_bytes = metadata.len;
        let cfb_path = checked_cfb_path(&path)?;
        let mut compound = self.compound.borrow_mut();
        let mut stream = compound.open_stream(cfb_path)?;
        Ok(partition::analyze_partition(
            path,
            &mut stream,
            stored_bytes,
            options,
        )?)
    }
}

impl RvtContainer {
    /// Inflate one partition member into memory, up to `limit` bytes.
    ///
    /// `logical_offset` is a member offset from [`PartitionReport`], measured
    /// in the checksum-clean stream.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent/non-partition stream, an unsafe path, an
    /// I/O failure, a member that is not valid gzip/DEFLATE at that offset, or
    /// a payload above `limit`.
    pub fn decode_partition_member(
        &self,
        name: &str,
        logical_offset: u64,
        limit: u64,
    ) -> Result<Vec<u8>> {
        let metadata = self.partition_metadata(name)?;
        let stored_bytes = metadata.len;
        let cfb_path = checked_cfb_path(&metadata.path)?;
        let mut compound = self.compound.borrow_mut();
        let mut stream = compound.open_stream(cfb_path)?;
        Ok(partition::decode_member_bytes(
            &mut stream,
            stored_bytes,
            logical_offset,
            limit,
        )?)
    }

    fn partition_metadata(&self, name: &str) -> Result<&StreamMetadata> {
        let metadata = self
            .stream(name)
            .ok_or_else(|| Error::StreamNotFound(name.to_owned()))?;
        if !metadata.path.starts_with("Partitions/")
            || metadata.path["Partitions/".len()..].contains('/')
        {
            return Err(Error::NotAPartitionStream(name.to_owned()));
        }
        Ok(metadata)
    }
}

fn checked_cfb_path(name: &str) -> Result<PathBuf> {
    let relative = name.trim_start_matches('/');
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::InvalidStreamPath(name.to_owned()));
    }
    Ok(Path::new("/").join(path))
}

fn display_stream_path(path: &Path) -> String {
    path.strip_prefix("/")
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn fixture() -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        {
            let mut compound = cfb::create(file.path()).unwrap();
            compound.create_storage("/Global").unwrap();
            compound.create_storage("/Partitions").unwrap();
            compound
                .create_stream("/BasicFileInfo")
                .unwrap()
                .write_all(b"info")
                .unwrap();
            compound
                .create_stream("/Global/Latest")
                .unwrap()
                .write_all(b"latest")
                .unwrap();
            compound
                .create_stream("/Partitions/42")
                .unwrap()
                .write_all(b"partition")
                .unwrap();
            compound.flush().unwrap();
        }
        file
    }

    #[test]
    fn enumerates_and_reads_streams() {
        let fixture = fixture();
        let container = RvtContainer::open(fixture.path()).unwrap();

        assert_eq!(container.streams().len(), 3);
        assert_eq!(container.partition_count(), 1);
        assert_eq!(container.read_stream("Global/Latest").unwrap(), b"latest");
        assert_eq!(container.read_stream("/BasicFileInfo").unwrap(), b"info");
    }

    #[test]
    fn rejects_non_cfb_input() {
        let mut fixture = NamedTempFile::new().unwrap();
        fixture.write_all(b"not an RVT").unwrap();
        assert!(matches!(
            RvtContainer::open(fixture.path()),
            Err(Error::InvalidContainerSignature)
        ));
    }

    #[test]
    fn reports_missing_stream() {
        let fixture = fixture();
        let container = RvtContainer::open(fixture.path()).unwrap();
        assert!(matches!(
            container.read_stream("missing"),
            Err(Error::StreamNotFound(_))
        ));
    }
}
