use std::io;

use thiserror::Error;

/// Errors produced while opening or reading an RVT container.
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("not a CFB/OLE file (expected D0 CF 11 E0 A1 B1 1A E1 signature)")]
    InvalidContainerSignature,

    #[error("stream not found: {0}")]
    StreamNotFound(String),

    #[error("path does not identify a stream: {0}")]
    NotAStream(String),

    #[error("path does not identify a direct Partitions/* stream: {0}")]
    NotAPartitionStream(String),

    #[error("invalid stream path: {0}")]
    InvalidStreamPath(String),

    #[error("malformed BasicFileInfo stream: {0}")]
    MalformedBasicFileInfo(&'static str),

    #[error("stream {name} is {size} bytes, exceeding the {limit}-byte read limit")]
    StreamTooLarge { name: String, size: u64, limit: u64 },

    #[error("invalid gzip framing: {0}")]
    InvalidGzip(&'static str),

    #[error("decoded stream exceeds the {limit}-byte output limit")]
    DecodedStreamTooLarge { limit: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
