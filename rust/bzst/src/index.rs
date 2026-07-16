//! The bzst index (subtype `0x02`): a jump table from uncompressed offsets to
//! compressed locations. It is the last frame in the file, self-locating from a
//! 12-byte EOF trailer, and is an accelerator — the same information is
//! reconstructible by a forward pass over the block-header frames
//! ([`Index::rebuild`]).

use std::io::{Read, Seek, SeekFrom};

use crate::frame::{block_on_disk_len, Frame, FrameReader};
use crate::memory::default_alloc_limit;
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
    /// Loads the index of a seekable stream via its EOF trailer. A missing EOF
    /// sentinel is [`BzstError::Truncated`] (data lost); a present-but-unreadable
    /// index is [`BzstError::CorruptIndex`] (block data still recoverable via
    /// [`Index::rebuild`]).
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
            // No EOF sentinel: the file is truncated or not a bzst stream at all.
            return Err(BzstError::Truncated);
        }
        // The sentinel says the file is complete, so from here a bad index is a
        // corrupt (recoverable) index, not lost data. Bound every read against the
        // file length so a bogus index_offset/size can't over-read or over-allocate.
        if index_offset > end || end - index_offset < 8 {
            return Err(BzstError::CorruptIndex);
        }
        r.seek(SeekFrom::Start(index_offset))?;
        let mut head = [0u8; 8];
        r.read_exact(&mut head)?;
        if u32::from_le_bytes(head[0..4].try_into().unwrap()) != STRUCTURAL_MAGIC {
            return Err(BzstError::CorruptIndex);
        }
        let size = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        // The index is the last frame, so it must span exactly to EOF.
        if 8 + size as u64 != end - index_offset {
            return Err(BzstError::CorruptIndex);
        }
        let mut frame = Vec::new();
        frame.try_reserve_exact(8 + size).map_err(|_| BzstError::CorruptIndex)?;
        frame.resize(8 + size, 0);
        frame[..8].copy_from_slice(&head);
        r.read_exact(&mut frame[8..])?;
        Self::parse_frame(&frame)
    }

    /// Reconstructs the index by a forward pass over the block-header frames
    /// (for an index-less or damaged file).
    pub fn rebuild<R: Read>(r: R) -> BzstResult<Self> {
        let mut fr = FrameReader::new(r, default_alloc_limit());
        let mut builder = IndexBuilder::new();
        loop {
            let start = fr.position();
            match fr.next_frame()? {
                None => break,
                Some(Frame::Block { header, .. }) => {
                    builder.push(
                        start,
                        block_on_disk_len(header.compressed_size),
                        header.uncompressed_size,
                    );
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
        Some(end.saturating_sub(start))
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
            return Err(BzstError::CorruptIndex);
        }
        let index_flags = f[9];
        let entry_count = u64::from_le_bytes(f[10..18].try_into().unwrap()) as usize;
        let total = u64::from_le_bytes(f[18..26].try_into().unwrap());
        let entries_len = entry_count.checked_mul(ENTRY_LEN).ok_or(BzstError::IndexTooLarge)?;
        let entries_end = HEAD.checked_add(entries_len).ok_or(BzstError::IndexTooLarge)?;
        let frame_min = entries_end.checked_add(FIXED_TAIL).ok_or(BzstError::IndexTooLarge)?;
        if f.len() < frame_min {
            return Err(BzstError::CorruptIndex);
        }
        let eof = u32::from_le_bytes(f[f.len() - 4..].try_into().unwrap());
        if eof != EOF_MAGIC {
            return Err(BzstError::CorruptIndex);
        }
        let stored = u64::from_le_bytes(f[entries_end..entries_end + 8].try_into().unwrap());
        if xxh64(&f[9..entries_end]) != stored {
            return Err(BzstError::CorruptIndex);
        }
        if index_flags != 0 {
            // A compressed index (Index_Flags bit 0) is a later-version feature we
            // can't decode; treat it as an unrecoverable index so seekable readers
            // fall back to rebuilding from the block-header frames.
            return Err(BzstError::CorruptIndex);
        }
        let mut entries = Vec::new();
        entries.try_reserve(entry_count).map_err(|_| BzstError::IndexTooLarge)?;
        for i in 0..entry_count {
            let b = HEAD + i * ENTRY_LEN;
            entries.push(IndexEntry {
                uncompressed_offset: u64::from_le_bytes(f[b..b + 8].try_into().unwrap()),
                block_offset: u64::from_le_bytes(f[b + 8..b + 16].try_into().unwrap()),
                block_length: u64::from_le_bytes(f[b + 16..b + 24].try_into().unwrap()),
            });
        }
        // The index must be able to map the whole uncompressed range, or the offset
        // arithmetic (block_for_offset, uncompressed_block_size) would misbehave or
        // silently drop data on a crafted index: an empty index must have zero
        // total; otherwise the first block starts at offset 0, offsets increase
        // strictly, and the last block starts before the end.
        let valid_shape = match entries.as_slice() {
            [] => total == 0,
            [first, ..] => {
                first.uncompressed_offset == 0
                    && entries
                        .windows(2)
                        .all(|w| w[0].uncompressed_offset < w[1].uncompressed_offset)
                    && entries.last().is_some_and(|e| e.uncompressed_offset < total)
            }
        };
        if !valid_shape {
            return Err(BzstError::CorruptIndex);
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

#[cfg(test)]
mod tests {
    use super::{Index, IndexEntry};
    use crate::{BzstError, EOF_MAGIC, STRUCTURAL_MAGIC, SUBTYPE_INDEX};

    #[test]
    fn crafted_entry_count_overflow_errors_not_panics() {
        // A minimal index frame whose entry_count would overflow the length
        // arithmetic; parsing must return IndexTooLarge, not panic.
        let mut f = vec![0u8; 46]; // HEAD(26) + FIXED_TAIL(20)
        f[0..4].copy_from_slice(&STRUCTURAL_MAGIC.to_le_bytes());
        f[4..8].copy_from_slice(&38u32.to_le_bytes());
        f[8] = SUBTYPE_INDEX;
        f[10..18].copy_from_slice(&(u64::MAX / 8).to_le_bytes()); // poisoned entry_count
        f[42..46].copy_from_slice(&EOF_MAGIC.to_le_bytes());
        assert!(matches!(Index::parse_frame(&f), Err(BzstError::IndexTooLarge)));
    }

    #[test]
    fn non_monotonic_index_is_rejected() {
        // A checksum-valid frame whose entries go backwards must be rejected so
        // the offset arithmetic can't underflow.
        let index = Index {
            entries: vec![
                IndexEntry { uncompressed_offset: 100, block_offset: 28, block_length: 50 },
                IndexEntry { uncompressed_offset: 50, block_offset: 78, block_length: 50 },
            ],
            total_uncompressed: 200,
        };
        let frame = index.to_frame_bytes(0).unwrap();
        assert!(matches!(Index::parse_frame(&frame), Err(BzstError::CorruptIndex)));
    }
}
