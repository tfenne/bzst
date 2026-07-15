//! # bzst — block-compressed zstd
//!
//! `bzst` ("beast") is a container format built on the Zstandard frame format
//! (RFC 8878). It stores a stream as a sequence of independently-compressed
//! *blocks* so that (de)compression can be parallelized, and it embeds a
//! self-contained index mapping uncompressed offsets to compressed offsets so
//! any position can be reached in `O(log n)`. A file written in the baseline
//! profile is a valid Zstandard archive: `zstd -d` reproduces the original
//! bytes.
//!
//! This crate implements the working-draft spec that lives in `spec/bzst.typ`.
//! Provisional wire constants (magic numbers) are flagged as such.
//!
//! ## Layers
//!
//! * High level: [`BzstWriter`] / [`BzstReader`] / [`SeekableReader`] — the
//!   `std::io` ergonomic surface, plus block management and threading.
//! * Value types: [`Header`], [`BlockHeader`], [`Index`], [`Frame`], … — the
//!   typed views of the wire format.
//! * The zstd codec and raw frame I/O are internal (`pub(crate)`); the public
//!   surface is the operations, not raw frame poking.

mod codec;
mod concat;
mod frame;
mod index;
mod reader;
mod writer;

pub use concat::concat;
pub use frame::{BlockFlags, BlockHeader, Frames, Header, OwnedFrame, Profiles};
pub use index::{Index, IndexEntry};
pub use reader::{BzstReader, BzstReaderBuilder, SeekableReader};
pub use writer::{BzstWriter, BzstWriterBuilder, Pool, Threads};

use std::fmt;
use std::io::{self, Read, Write};

// --- wire constants --------------------------------------------------------

/// bzst structural skippable-frame magic. **Provisional** (spec open issue §10.3).
pub const STRUCTURAL_MAGIC: u32 = 0x184D_2A5B;
/// EOF sentinel; the last four bytes of a complete file. **Provisional**.
pub const EOF_MAGIC: u32 = 0x8F92_EA5B;
/// The four ASCII bytes identifying the container as bzst (`"BZST"`).
pub const SIGNATURE: [u8; 4] = *b"BZST";
/// bzst format version implemented by this crate.
pub const FORMAT_VERSION: u8 = 1;

/// Structural-frame subtype: file header.
pub const SUBTYPE_HEADER: u8 = 0x00;
/// Structural-frame subtype: per-block header (sizes + flags).
pub const SUBTYPE_BLOCK_HEADER: u8 = 0x01;
/// Structural-frame subtype: the uncompressed→compressed index.
pub const SUBTYPE_INDEX: u8 = 0x02;
/// Structural-frame subtype: reserved for an embedded dictionary (unused in v1).
pub const SUBTYPE_DICTIONARY: u8 = 0x03;

/// Low bound of the zstd skippable-frame magic range.
pub const SKIPPABLE_MAGIC_MIN: u32 = 0x184D_2A50;
/// High bound of the zstd skippable-frame magic range.
pub const SKIPPABLE_MAGIC_MAX: u32 = 0x184D_2A5F;
/// The zstd data-frame magic.
pub const ZSTD_FRAME_MAGIC: u32 = 0xFD2F_B528;

/// Default per-block uncompressed size (1 MiB).
pub const DEFAULT_BLOCK_SIZE: usize = 1 << 20;
/// Default zstd compression level (matches `ZSTD_CLEVEL_DEFAULT`).
pub const DEFAULT_LEVEL: i32 = 3;

// --- result & error --------------------------------------------------------

/// Result type used throughout the crate.
pub type BzstResult<T> = std::result::Result<T, BzstError>;

/// Errors produced by bzst operations.
#[non_exhaustive]
#[derive(Debug)]
pub enum BzstError {
    /// An underlying I/O (or zstd) error.
    Io(io::Error),
    /// A frame magic number did not match what was expected.
    BadMagic { expected: u32, found: u32 },
    /// The file declares a bzst format version this build does not understand.
    UnsupportedVersion(u8),
    /// A structural frame's checksum did not validate.
    ChecksumMismatch { frame: &'static str },
    /// The stream ended in the middle of a frame, or lacks a valid EOF trailer.
    Truncated,
    /// A caller-supplied skippable magic is out of range or collides with bzst's own.
    BadSkippableMagic(u32),
    /// The index does not fit in a single skippable frame (billions of blocks).
    IndexTooLarge,
    /// A worker thread pool could not be constructed.
    Thread(String),
}

impl fmt::Display for BzstError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BzstError::Io(e) => write!(f, "io error: {e}"),
            BzstError::BadMagic { expected, found } => {
                write!(f, "bad magic: expected {expected:#010x}, found {found:#010x}")
            }
            BzstError::UnsupportedVersion(v) => write!(f, "unsupported bzst format version {v}"),
            BzstError::ChecksumMismatch { frame } => write!(f, "{frame} frame checksum mismatch"),
            BzstError::Truncated => write!(f, "truncated or incomplete bzst stream"),
            BzstError::BadSkippableMagic(m) => {
                write!(f, "skippable magic {m:#010x} is out of range or reserved by bzst")
            }
            BzstError::IndexTooLarge => write!(f, "index too large for a single frame"),
            BzstError::Thread(msg) => write!(f, "thread pool error: {msg}"),
        }
    }
}

impl std::error::Error for BzstError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BzstError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for BzstError {
    fn from(e: io::Error) -> Self {
        BzstError::Io(e)
    }
}

impl From<BzstError> for io::Error {
    fn from(e: BzstError) -> Self {
        match e {
            BzstError::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}

// --- free functions --------------------------------------------------------

/// Returns `true` if `prefix` looks like the start of a bzst file: a zstd
/// skippable magic at offset 0 and the `"BZST"` signature at offset 10.
pub fn detect(prefix: &[u8]) -> bool {
    if prefix.len() < 14 {
        return false;
    }
    let magic = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    (SKIPPABLE_MAGIC_MIN..=SKIPPABLE_MAGIC_MAX).contains(&magic) && &prefix[10..14] == b"BZST"
}

/// Reads and returns the [`Header`] frame at the start of a bzst stream.
pub fn header_of<R: Read>(mut r: R) -> BzstResult<Header> {
    Header::read_from(&mut r)
}

/// Compresses a whole buffer into a single-shot bzst archive at `level`.
pub fn compress(src: &[u8], level: i32) -> BzstResult<Vec<u8>> {
    let mut w = BzstWriter::builder(Vec::new()).level(level).build()?;
    w.write_all(src)?;
    w.finish()
}

/// Decompresses a whole bzst archive into a `Vec<u8>`.
pub fn decompress(src: &[u8]) -> BzstResult<Vec<u8>> {
    let mut out = Vec::new();
    BzstReader::new(io::Cursor::new(src))?.read_to_end(&mut out)?;
    Ok(out)
}

// XXH64 over `bytes`, seed 0 — the hash used for all structural-frame checksums.
pub(crate) fn xxh64(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh64::xxh64(bytes, 0)
}
