//! The bzst index (subtype `0x02`): a jump table from uncompressed offsets to
//! compressed locations. It is the last frame in the file, self-locating from a
//! 12-byte EOF trailer, and is an accelerator — the same information is
//! reconstructible by a forward pass over the block-header frames
//! ([`Index::rebuild`]).

use std::io::{Read, Seek, SeekFrom};

use crate::frame::{Frame, FrameReader, BLOCK_HEADER_FRAME_LEN};
use crate::{xxh64, BzstError, BzstResult, EOF_MAGIC, STRUCTURAL_MAGIC, SUBTYPE_INDEX};

/// Bytes per on-disk index entry (three `u64`s).
const ENTRY_LEN: usize = 24;
/// Fixed bytes after the entries: checksum(8) + index_offset(8) + eof_magic(4).
const FIXED_TAIL: usize = 8 + 8 + 4;
/// Bytes of the EOF trailer (index_offset + eof_magic).
pub(crate) const EOF_TRAILER_LEN: usize = 12;

/// A single index entry: where a block lives and what uncompressed offset it
/// begins at. `block_offset` points at the block's *block-header frame*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// Uncompressed byte offset at which this block's decoded data begins.
    pub uncompressed_offset: u64,
    /// Absolute file offset of this block's block-header frame.
    pub block_offset: u64,
    /// On-disk length of `[block-header frame][data frame]`.
    pub block_length: u64,
}

/// The parsed index: an immutable, `Sync` jump table over a file's blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    entries: Vec<IndexEntry>,
    total_uncompressed: u64,
}

impl Index {
    /// Loads the index of a seekable stream via its EOF trailer.
    pub fn read_from<R: Read + Seek>(r: &mut R) -> BzstResult<Self> {
        let end = r.seek(SeekFrom::End(0))?;
        if end < EOF_TRAILER_LEN as u64 {
            return Err(BzstError::Truncated);
        }
        r.seek(SeekFrom::End(-(EOF_TRAILER_LEN as i64)))?;
        let mut trailer = [0u8; EOF_TRAILER_LEN];
        r.read_exact(&mut trailer)?;
        let index_offset = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let eof = u32::from_le_bytes(trailer[8..12].try_into().unwrap());
        if eof != EOF_MAGIC {
            return Err(BzstError::BadMagic { expected: EOF_MAGIC, found: eof });
        }
        r.seek(SeekFrom::Start(index_offset))?;
        let mut head = [0u8; 8];
        r.read_exact(&mut head)?;
        let magic = u32::from_le_bytes(head[0..4].try_into().unwrap());
        if magic != STRUCTURAL_MAGIC {
            return Err(BzstError::BadMagic { expected: STRUCTURAL_MAGIC, found: magic });
        }
        let size = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        let mut frame = Vec::with_capacity(8 + size);
        frame.extend_from_slice(&head);
        frame.resize(8 + size, 0);
        r.read_exact(&mut frame[8..])?;
        Self::parse_frame(&frame)
    }

    /// Reconstructs the index by a forward pass over the block-header frames
    /// (for an index-less or damaged file).
    pub fn rebuild<R: Read>(r: R) -> BzstResult<Self> {
        let mut fr = FrameReader::new(r);
        let mut builder = IndexBuilder::new();
        loop {
            let start = fr.position();
            match fr.next_frame()? {
                None => break,
                Some(Frame::Block { header, .. }) => {
                    let block_length = BLOCK_HEADER_FRAME_LEN as u64 + header.compressed_size;
                    builder.push(start, block_length, header.uncompressed_size);
                }
                Some(_) => {}
            }
        }
        Ok(builder.finish())
    }

    /// Number of blocks in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if there are no blocks.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The index entries, in block order.
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// The `i`th entry, if present.
    pub fn entry(&self, i: usize) -> Option<&IndexEntry> {
        self.entries.get(i)
    }

    /// Total uncompressed size of the file.
    pub fn total_uncompressed(&self) -> u64 {
        self.total_uncompressed
    }

    /// Index of the block containing uncompressed byte `offset`, if in range.
    pub fn block_for_offset(&self, offset: u64) -> Option<usize> {
        if offset >= self.total_uncompressed {
            return None;
        }
        let count = self.entries.partition_point(|e| e.uncompressed_offset <= offset);
        (count > 0).then(|| count - 1)
    }

    /// Uncompressed size of block `i` — the distance from its start to the next
    /// block's start, or to the end of the stream for the last block. Sizes are
    /// not stored per block; they are derived from consecutive offsets. `None` if
    /// `i` is out of range.
    pub fn uncompressed_block_size(&self, i: usize) -> Option<u64> {
        let start = self.entries.get(i)?.uncompressed_offset;
        let end = match self.entries.get(i + 1) {
            Some(next) => next.uncompressed_offset,
            None => self.total_uncompressed,
        };
        Some(end - start)
    }

    pub(crate) fn parse_frame(f: &[u8]) -> BzstResult<Self> {
        const HEAD: usize = 26; // magic(4)+size(4)+subtype(1)+flags(1)+count(8)+total(8)
        if f.len() < HEAD + FIXED_TAIL {
            return Err(BzstError::Truncated);
        }
        let magic = u32::from_le_bytes(f[0..4].try_into().unwrap());
        if magic != STRUCTURAL_MAGIC {
            return Err(BzstError::BadMagic { expected: STRUCTURAL_MAGIC, found: magic });
        }
        if f[8] != SUBTYPE_INDEX {
            return Err(BzstError::Truncated);
        }
        let index_flags = f[9];
        let entry_count = u64::from_le_bytes(f[10..18].try_into().unwrap()) as usize;
        let total = u64::from_le_bytes(f[18..26].try_into().unwrap());
        let entries_len = entry_count.checked_mul(ENTRY_LEN).ok_or(BzstError::IndexTooLarge)?;
        let entries_end = HEAD.checked_add(entries_len).ok_or(BzstError::IndexTooLarge)?;
        if f.len() < entries_end + FIXED_TAIL {
            return Err(BzstError::Truncated);
        }
        let eof = u32::from_le_bytes(f[f.len() - 4..].try_into().unwrap());
        if eof != EOF_MAGIC {
            return Err(BzstError::BadMagic { expected: EOF_MAGIC, found: eof });
        }
        let stored = u64::from_le_bytes(f[entries_end..entries_end + 8].try_into().unwrap());
        if xxh64(&f[9..entries_end]) != stored {
            return Err(BzstError::ChecksumMismatch { frame: "index" });
        }
        if index_flags != 0 {
            // Compressed-index entries are reserved for a later version.
            return Err(BzstError::Truncated);
        }
        let mut entries = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let b = HEAD + i * ENTRY_LEN;
            entries.push(IndexEntry {
                uncompressed_offset: u64::from_le_bytes(f[b..b + 8].try_into().unwrap()),
                block_offset: u64::from_le_bytes(f[b + 8..b + 16].try_into().unwrap()),
                block_length: u64::from_le_bytes(f[b + 16..b + 24].try_into().unwrap()),
            });
        }
        Ok(Self { entries, total_uncompressed: total })
    }

    pub(crate) fn to_frame_bytes(&self, index_offset: u64) -> BzstResult<Vec<u8>> {
        let entries_len =
            self.entries.len().checked_mul(ENTRY_LEN).ok_or(BzstError::IndexTooLarge)?;
        // Checksummed region: index_flags + entry_count + total + entries.
        let mut checked = Vec::with_capacity(1 + 8 + 8 + entries_len);
        checked.push(0u8); // index_flags: uncompressed entries in v1
        checked.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        checked.extend_from_slice(&self.total_uncompressed.to_le_bytes());
        for e in &self.entries {
            checked.extend_from_slice(&e.uncompressed_offset.to_le_bytes());
            checked.extend_from_slice(&e.block_offset.to_le_bytes());
            checked.extend_from_slice(&e.block_length.to_le_bytes());
        }
        let cksum = xxh64(&checked);
        let frame_size = 1 + checked.len() + FIXED_TAIL; // subtype + checked + tail
        let frame_size: u32 = frame_size.try_into().map_err(|_| BzstError::IndexTooLarge)?;

        let mut f = Vec::with_capacity(8 + frame_size as usize);
        f.extend_from_slice(&STRUCTURAL_MAGIC.to_le_bytes());
        f.extend_from_slice(&frame_size.to_le_bytes());
        f.push(SUBTYPE_INDEX);
        f.extend_from_slice(&checked);
        f.extend_from_slice(&cksum.to_le_bytes());
        f.extend_from_slice(&index_offset.to_le_bytes());
        f.extend_from_slice(&EOF_MAGIC.to_le_bytes());
        Ok(f)
    }
}

/// Accumulates index entries as blocks are written, tracking the running
/// uncompressed offset.
pub(crate) struct IndexBuilder {
    entries: Vec<IndexEntry>,
    uncompressed: u64,
}

impl IndexBuilder {
    pub(crate) fn new() -> Self {
        Self { entries: Vec::new(), uncompressed: 0 }
    }

    pub(crate) fn push(&mut self, block_offset: u64, block_length: u64, uncompressed_size: u64) {
        self.entries.push(IndexEntry {
            uncompressed_offset: self.uncompressed,
            block_offset,
            block_length,
        });
        self.uncompressed += uncompressed_size;
    }

    pub(crate) fn finish(self) -> Index {
        Index { entries: self.entries, total_uncompressed: self.uncompressed }
    }
}
