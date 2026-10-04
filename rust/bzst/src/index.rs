//! The bzst index (subtype `0x02`): a jump table from uncompressed offsets to
//! compressed locations. It is the last frame in the file, self-locating from a
//! 12-byte EOF trailer, and is an accelerator — the same information is
//! reconstructible by a forward pass over the block-header frames
//! ([`Index::rebuild`]).
//!
//! On disk the entries are split into partitions of consecutive blocks. Each
//! partition is one zstd frame holding three `u32` columns (uncompressed size,
//! derived-frames length, block length) from which offsets are rebuilt with
//! running sums. A fixed-width directory of the partitions, the counts and the
//! trailer sit at the very end of the frame, so one read of the file's tail
//! finds everything needed to plan a lookup. [`Index`] decodes every partition;
//! [`LazyIndex`] reads only the tail and decodes partitions on demand.

use std::io::{Read, Seek, SeekFrom};

use crate::codec::{ZstdCompressor, ZstdDecompressor};
use crate::frame::{block_on_disk_len, Frame, FrameReader, HEADER_FRAME_LEN};
use crate::memory::default_alloc_limit;
use crate::{
    crc32, BzstError, BzstResult, DEFAULT_LEVEL, EOF_MAGIC, STRUCTURAL_MAGIC, SUBTYPE_INDEX,
    ZSTD_FRAME_MAGIC,
};

/// Leading bytes of the index frame: magic(4) + frame_size(4) + subtype(1). The
/// first partition starts immediately after.
const HEAD_LEN: usize = 4 + 4 + 1;
/// Bytes per directory entry: first uncompressed offset(8) + first block
/// offset(8) + partition offset(8) + partition length(4) + entry count(4).
const DIRECTORY_ENTRY_LEN: usize = 32;
/// Fixed fields between the directory and the checksum, which the checksum also
/// covers: entry_count(8) + total_uncompressed(8) + blocks_end(8) +
/// partition_count(4).
const COUNTS_LEN: usize = 8 + 8 + 8 + 4;
/// Bytes of the EOF trailer (index_offset + eof_magic).
pub(crate) const EOF_TRAILER_LEN: usize = 12;
/// Everything after the directory: counts + checksum(4) + EOF trailer.
const TAIL_LEN: usize = COUNTS_LEN + 4 + EOF_TRAILER_LEN;
/// Decompressed bytes per block in a partition: one `u32` in each of three columns.
const PARTITION_BYTES_PER_ENTRY: usize = 12;
/// How much of the file's end [`LazyIndex::open`] reads at once: enough for the
/// tail and the directory of up to 2,046 partitions (over 8M blocks at the
/// default partition size), so typically a lookup needs just one more read.
const TAIL_READ_LEN: usize = 64 << 10;
/// zstd `Frame_Header_Descriptor` bit for the content checksum (RFC 8878).
const ZSTD_CONTENT_CHECKSUM_FLAG: u8 = 0x04;

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
        Self::parse_frame(&frame, index_offset)
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
                        u64::from(header.uncompressed_size),
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

    /// File offset just past the last block (the end of the header frame if there
    /// are no blocks). Frames a derived format writes after its last block lie
    /// between here and the index frame.
    pub fn blocks_end(&self) -> u64 {
        self.entries.last().map_or(HEADER_FRAME_LEN as u64, |e| e.block_offset + e.block_length)
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
    /// block's start, or to the end of the stream for the last block. `None` if
    /// `i` is out of range.
    pub fn uncompressed_block_size(&self, i: usize) -> Option<u64> {
        let start = self.entries.get(i)?.uncompressed_offset;
        let end = match self.entries.get(i + 1) {
            Some(next) => next.uncompressed_offset,
            None => self.total_uncompressed,
        };
        Some(end.saturating_sub(start))
    }

    /// Parses and fully validates an index frame that begins at file offset
    /// `frame_offset`, decoding every partition.
    pub(crate) fn parse_frame(f: &[u8], frame_offset: u64) -> BzstResult<Self> {
        if f.len() < HEAD_LEN + TAIL_LEN {
            return Err(BzstError::Truncated);
        }
        let magic = u32::from_le_bytes(f[0..4].try_into().unwrap());
        if magic != STRUCTURAL_MAGIC {
            return Err(BzstError::BadMagic { expected: STRUCTURAL_MAGIC, found: magic });
        }
        let frame_size = u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize;
        if f[8] != SUBTYPE_INDEX || frame_size + 8 != f.len() {
            return Err(BzstError::CorruptIndex);
        }
        let tail = IndexTail::parse(&f[f.len() - TAIL_LEN..]);
        if tail.eof_magic != EOF_MAGIC || tail.index_offset != frame_offset {
            return Err(BzstError::CorruptIndex);
        }
        let directory_start = (f.len() - TAIL_LEN)
            .checked_sub(tail.directory_len()?)
            .filter(|&start| start >= HEAD_LEN)
            .ok_or(BzstError::CorruptIndex)?;
        let directory = tail.verify_and_parse_directory(&f[directory_start..])?;
        validate_directory(
            &directory,
            &tail,
            frame_offset + HEAD_LEN as u64,
            frame_offset + directory_start as u64,
        )?;

        let mut entries = Vec::new();
        let mut dec = ZstdDecompressor::new()?;
        for (i, partition) in directory.iter().enumerate() {
            // Validated contiguous within [HEAD_LEN, directory_start) above.
            let start = (partition.offset - frame_offset) as usize;
            let raw = &f[start..start + partition.length as usize];
            partition.decode(&mut dec, raw, directory.get(i + 1), &tail, &mut entries)?;
        }
        Ok(Self { entries, total_uncompressed: tail.total_uncompressed })
    }

    /// Encodes the index as a frame that will begin at file offset `index_offset`,
    /// with up to `partition_entries` blocks per partition.
    pub(crate) fn to_frame_bytes(
        &self,
        index_offset: u64,
        partition_entries: usize,
    ) -> BzstResult<Vec<u8>> {
        let partition_entries = partition_entries.clamp(1, u32::MAX as usize);
        let mut f = Vec::new();
        f.extend_from_slice(&STRUCTURAL_MAGIC.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes()); // Frame_Size, filled in at the end
        f.push(SUBTYPE_INDEX);

        let mut zc = ZstdCompressor::new(DEFAULT_LEVEL, true)?;
        let mut directory = Vec::new();
        for (i, chunk) in self.entries.chunks(partition_entries).enumerate() {
            let columns = self.partition_columns(i * partition_entries, chunk.len())?;
            let mut compressed = vec![0u8; ZstdCompressor::bound(columns.len())];
            let n = zc.compress(&columns, &mut compressed)?;
            directory.push(Partition {
                first_uncompressed_offset: chunk[0].uncompressed_offset,
                first_block_offset: chunk[0].block_offset,
                offset: index_offset + f.len() as u64,
                length: u32::try_from(n).map_err(|_| BzstError::IndexTooLarge)?,
                entry_count: chunk.len() as u32,
            });
            f.extend_from_slice(&compressed[..n]);
        }

        let checked_start = f.len();
        for partition in &directory {
            partition.write_to(&mut f);
        }
        let partition_count =
            u32::try_from(directory.len()).map_err(|_| BzstError::IndexTooLarge)?;
        f.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        f.extend_from_slice(&self.total_uncompressed.to_le_bytes());
        f.extend_from_slice(&self.blocks_end().to_le_bytes());
        f.extend_from_slice(&partition_count.to_le_bytes());
        // The checksum covers the directory and counts: the region a reader gets
        // from one read of the file's tail. Partitions carry zstd content checksums.
        let checksum = crc32(&f[checked_start..]);
        f.extend_from_slice(&checksum.to_le_bytes());
        f.extend_from_slice(&index_offset.to_le_bytes());
        f.extend_from_slice(&EOF_MAGIC.to_le_bytes());

        let frame_size = u32::try_from(f.len() - 8).map_err(|_| BzstError::IndexTooLarge)?;
        f[4..8].copy_from_slice(&frame_size.to_le_bytes());
        Ok(f)
    }

    /// The decompressed content of the partition holding the `count` entries from
    /// `start`: three `u32` columns, stored one after another — each block's
    /// uncompressed size, the bytes of other frames between it and the previous
    /// block (zero for the partition's first block), and its on-disk length.
    fn partition_columns(&self, start: usize, count: usize) -> BzstResult<Vec<u8>> {
        let mut columns = vec![0u8; count * PARTITION_BYTES_PER_ENTRY];
        let (sizes, rest) = columns.split_at_mut(count * 4);
        let (derived_lengths, block_lengths) = rest.split_at_mut(count * 4);
        for i in 0..count {
            let entry = &self.entries[start + i];
            let next_offset = self
                .entries
                .get(start + i + 1)
                .map_or(self.total_uncompressed, |next| next.uncompressed_offset);
            let size = next_offset
                .checked_sub(entry.uncompressed_offset)
                .filter(|&size| size > 0)
                .ok_or(BzstError::Malformed("index entries must have increasing offsets"))?;
            let derived_length = match i {
                0 => 0,
                _ => {
                    let prev = &self.entries[start + i - 1];
                    entry
                        .block_offset
                        .checked_sub(prev.block_offset + prev.block_length)
                        .ok_or(BzstError::Malformed("index entries overlap on disk"))?
                }
            };
            let slot = i * 4..i * 4 + 4;
            sizes[slot.clone()].copy_from_slice(&to_u32(size, "a block's uncompressed size")?);
            derived_lengths[slot.clone()]
                .copy_from_slice(&to_u32(derived_length, "the frames between two blocks")?);
            block_lengths[slot]
                .copy_from_slice(&to_u32(entry.block_length, "a block's on-disk length")?);
        }
        Ok(columns)
    }
}

/// Random access into a seekable file's index without loading all of it.
/// Opening reads only the end of the file (the trailer, counts and partition
/// directory); each lookup then reads and decodes at most one partition, keeping
/// the most recent one cached. On high-latency storage that is two small reads
/// before a block can be fetched, however large the index.
pub struct LazyIndex<R> {
    inner: R,
    directory: Vec<Partition>,
    tail: IndexTail,
    dec: ZstdDecompressor,
    cached: Option<(usize, Vec<IndexEntry>)>,
}

impl<R: Read + Seek> LazyIndex<R> {
    /// Opens the index of `inner` by reading the end of the file. A missing EOF
    /// sentinel is [`BzstError::Truncated`]; an inconsistent tail or directory is
    /// [`BzstError::CorruptIndex`].
    pub fn open(inner: R) -> BzstResult<Self> {
        Self::open_with_tail_read(inner, TAIL_READ_LEN)
    }

    /// Number of blocks in the index.
    pub fn len(&self) -> u64 {
        self.tail.entry_count
    }

    /// True if there are no blocks.
    pub fn is_empty(&self) -> bool {
        self.tail.entry_count == 0
    }

    /// Total uncompressed size of the file.
    pub fn total_uncompressed(&self) -> u64 {
        self.tail.total_uncompressed
    }

    /// File offset just past the last block (the end of the header frame if there
    /// are no blocks). With [`LazyIndex::index_offset`] it brackets the frames a
    /// derived format wrote after its last block.
    pub fn blocks_end(&self) -> u64 {
        self.tail.blocks_end
    }

    /// File offset of the index frame.
    pub fn index_offset(&self) -> u64 {
        self.tail.index_offset
    }

    /// Number of index partitions.
    pub fn partition_count(&self) -> usize {
        self.directory.len()
    }

    /// The entry for the block containing uncompressed byte `offset`, reading and
    /// decoding that block's partition unless it is the cached one. `None` if
    /// `offset` is at or past the end of the data.
    pub fn entry_for_offset(&mut self, offset: u64) -> BzstResult<Option<IndexEntry>> {
        if offset >= self.tail.total_uncompressed {
            return Ok(None);
        }
        // The first partition starts at offset 0 (validated), so this is >= 1.
        let p = self.directory.partition_point(|d| d.first_uncompressed_offset <= offset) - 1;
        let entries = self.partition(p)?;
        let i = entries.partition_point(|e| e.uncompressed_offset <= offset) - 1;
        Ok(Some(entries[i]))
    }

    /// Returns the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Opens the index reading `tail_read` bytes from the end of the file first,
    /// then the remainder of the directory if it did not fit.
    fn open_with_tail_read(mut inner: R, tail_read: usize) -> BzstResult<Self> {
        let end = inner.seek(SeekFrom::End(0))?;
        if end < EOF_TRAILER_LEN as u64 {
            return Err(BzstError::Truncated);
        }
        let first_read = (tail_read.max(TAIL_LEN) as u64).min(end);
        let first_read_start = end - first_read;
        let mut buf = vec![0u8; first_read as usize];
        inner.seek(SeekFrom::Start(first_read_start))?;
        inner.read_exact(&mut buf)?;
        if u32::from_le_bytes(buf[buf.len() - 4..].try_into().unwrap()) != EOF_MAGIC {
            return Err(BzstError::Truncated);
        }
        if buf.len() < TAIL_LEN {
            return Err(BzstError::CorruptIndex);
        }
        let tail = IndexTail::parse(&buf[buf.len() - TAIL_LEN..]);

        // The directory sits just before the tail; check it fits after the frame's
        // head and in memory before reading, so a forged count can't drive a huge read.
        let directory_len = tail.directory_len()? as u64;
        if directory_len > default_alloc_limit() {
            return Err(BzstError::IndexTooLarge);
        }
        let directory_start = (end - TAIL_LEN as u64)
            .checked_sub(directory_len)
            .filter(|&start| start >= tail.index_offset.saturating_add(HEAD_LEN as u64))
            .ok_or(BzstError::CorruptIndex)?;
        let checked = if directory_start >= first_read_start {
            buf[(directory_start - first_read_start) as usize..].to_vec()
        } else {
            let mut checked = vec![0u8; (first_read_start - directory_start) as usize];
            inner.seek(SeekFrom::Start(directory_start))?;
            inner.read_exact(&mut checked)?;
            checked.extend_from_slice(&buf);
            checked
        };
        let directory = tail.verify_and_parse_directory(&checked)?;
        validate_directory(
            &directory,
            &tail,
            tail.index_offset + HEAD_LEN as u64,
            directory_start,
        )?;
        Ok(Self { inner, directory, tail, dec: ZstdDecompressor::new()?, cached: None })
    }

    /// The decoded entries of partition `p`, reading it unless it is cached.
    fn partition(&mut self, p: usize) -> BzstResult<&[IndexEntry]> {
        if !matches!(self.cached, Some((cached, _)) if cached == p) {
            let partition = self.directory[p];
            let mut raw = vec![0u8; partition.length as usize];
            self.inner.seek(SeekFrom::Start(partition.offset))?;
            self.inner.read_exact(&mut raw)?;
            let mut entries = Vec::new();
            partition.decode(
                &mut self.dec,
                &raw,
                self.directory.get(p + 1),
                &self.tail,
                &mut entries,
            )?;
            self.cached = Some((p, entries));
        }
        Ok(&self.cached.as_ref().expect("partition was just cached").1)
    }
}

/// The fixed fields at the very end of the index frame (and of the file).
#[derive(Debug, Clone, Copy)]
struct IndexTail {
    entry_count: u64,
    total_uncompressed: u64,
    blocks_end: u64,
    partition_count: u32,
    checksum: u32,
    index_offset: u64,
    eof_magic: u32,
}

impl IndexTail {
    /// Reads the fields from the last [`TAIL_LEN`] bytes of the frame. Validating
    /// them is up to the caller.
    fn parse(t: &[u8]) -> Self {
        Self {
            entry_count: u64::from_le_bytes(t[0..8].try_into().unwrap()),
            total_uncompressed: u64::from_le_bytes(t[8..16].try_into().unwrap()),
            blocks_end: u64::from_le_bytes(t[16..24].try_into().unwrap()),
            partition_count: u32::from_le_bytes(t[24..28].try_into().unwrap()),
            checksum: u32::from_le_bytes(t[28..32].try_into().unwrap()),
            index_offset: u64::from_le_bytes(t[32..40].try_into().unwrap()),
            eof_magic: u32::from_le_bytes(t[40..44].try_into().unwrap()),
        }
    }

    /// Byte length of the directory this tail describes.
    fn directory_len(&self) -> BzstResult<usize> {
        (self.partition_count as usize)
            .checked_mul(DIRECTORY_ENTRY_LEN)
            .ok_or(BzstError::CorruptIndex)
    }

    /// Verifies the checksum over `region` (the directory through the end of the
    /// frame) and parses the directory entries.
    fn verify_and_parse_directory(&self, region: &[u8]) -> BzstResult<Vec<Partition>> {
        let directory_len = self.directory_len()?;
        if region.len() != directory_len + TAIL_LEN
            || crc32(&region[..directory_len + COUNTS_LEN]) != self.checksum
        {
            return Err(BzstError::CorruptIndex);
        }
        Ok(region[..directory_len]
            .chunks_exact(DIRECTORY_ENTRY_LEN)
            .map(Partition::parse)
            .collect())
    }
}

/// One directory entry: where a partition's zstd frame lives and which blocks it
/// describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Partition {
    first_uncompressed_offset: u64,
    first_block_offset: u64,
    /// Absolute file offset of the partition's zstd frame.
    offset: u64,
    /// On-disk length of the partition's zstd frame.
    length: u32,
    entry_count: u32,
}

impl Partition {
    fn parse(b: &[u8]) -> Self {
        Self {
            first_uncompressed_offset: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            first_block_offset: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            offset: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            length: u32::from_le_bytes(b[24..28].try_into().unwrap()),
            entry_count: u32::from_le_bytes(b[28..32].try_into().unwrap()),
        }
    }

    fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.first_uncompressed_offset.to_le_bytes());
        out.extend_from_slice(&self.first_block_offset.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out.extend_from_slice(&self.entry_count.to_le_bytes());
    }

    /// Decompresses this partition's zstd frame `raw` and appends its entries to
    /// `out`, checking them against the next partition (or, for the last, against
    /// the end of the data and the start of the index frame).
    fn decode(
        &self,
        dec: &mut ZstdDecompressor,
        raw: &[u8],
        next: Option<&Partition>,
        tail: &IndexTail,
        out: &mut Vec<IndexEntry>,
    ) -> BzstResult<()> {
        let count = self.entry_count as usize;
        let expected_len = count * PARTITION_BYTES_PER_ENTRY;
        // The frame must carry a content checksum, so corruption inside it is
        // caught, and must declare exactly the expected size, so a forged count
        // is rejected before the output buffer is allocated.
        let has_checksum = raw.len() > 4
            && u32::from_le_bytes(raw[0..4].try_into().unwrap()) == ZSTD_FRAME_MAGIC
            && raw[4] & ZSTD_CONTENT_CHECKSUM_FLAG != 0;
        let declared_len = zstd::zstd_safe::get_frame_content_size(raw).ok().flatten();
        if !has_checksum || declared_len != Some(expected_len as u64) {
            return Err(BzstError::CorruptIndex);
        }
        if expected_len as u64 > default_alloc_limit() {
            return Err(BzstError::IndexTooLarge);
        }
        let mut columns = vec![0u8; expected_len];
        match dec.decompress(raw, &mut columns) {
            Ok(n) if n == expected_len => {}
            _ => return Err(BzstError::CorruptIndex),
        }

        let column = |c: usize, i: usize| {
            let at = (c * count + i) * 4;
            u64::from(u32::from_le_bytes(columns[at..at + 4].try_into().unwrap()))
        };
        out.try_reserve(count).map_err(|_| BzstError::IndexTooLarge)?;
        let mut uncompressed_offset = self.first_uncompressed_offset;
        let mut block_offset = self.first_block_offset;
        for i in 0..count {
            let (size, derived_length, block_length) = (column(0, i), column(1, i), column(2, i));
            if size == 0 || (i == 0 && derived_length != 0) {
                return Err(BzstError::CorruptIndex);
            }
            block_offset =
                block_offset.checked_add(derived_length).ok_or(BzstError::CorruptIndex)?;
            out.push(IndexEntry { uncompressed_offset, block_offset, block_length });
            uncompressed_offset =
                uncompressed_offset.checked_add(size).ok_or(BzstError::CorruptIndex)?;
            block_offset = block_offset.checked_add(block_length).ok_or(BzstError::CorruptIndex)?;
        }
        // The sizes must reach exactly where the next partition (or the data)
        // begins, and the blocks must end no later than the next partition's first
        // block, or, for the last partition, exactly at Blocks_End.
        let (uncompressed_end, blocks_end_valid) = match next {
            Some(next) => (next.first_uncompressed_offset, block_offset <= next.first_block_offset),
            None => (tail.total_uncompressed, block_offset == tail.blocks_end),
        };
        if uncompressed_offset != uncompressed_end || !blocks_end_valid {
            return Err(BzstError::CorruptIndex);
        }
        Ok(())
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

/// Checks the directory's internal consistency: partitions are non-empty and
/// contiguous from `partitions_start` to `directory_start`, their counts sum to
/// the index's, their first offsets start at zero and increase, and the blocks
/// end before the index frame begins.
fn validate_directory(
    directory: &[Partition],
    tail: &IndexTail,
    partitions_start: u64,
    directory_start: u64,
) -> BzstResult<()> {
    if tail.blocks_end > tail.index_offset {
        return Err(BzstError::CorruptIndex);
    }
    let Some(first) = directory.first() else {
        return match tail.entry_count == 0 && tail.total_uncompressed == 0 {
            true => Ok(()),
            false => Err(BzstError::CorruptIndex),
        };
    };
    let mut expected_offset = partitions_start;
    let mut entries = 0u64;
    for partition in directory {
        if partition.offset != expected_offset || partition.entry_count == 0 {
            return Err(BzstError::CorruptIndex);
        }
        expected_offset = expected_offset
            .checked_add(u64::from(partition.length))
            .ok_or(BzstError::CorruptIndex)?;
        entries += u64::from(partition.entry_count);
    }
    let increasing = directory.windows(2).all(|w| {
        w[0].first_uncompressed_offset < w[1].first_uncompressed_offset
            && w[0].first_block_offset < w[1].first_block_offset
    });
    let valid = expected_offset == directory_start
        && entries == tail.entry_count
        && first.first_uncompressed_offset == 0
        && increasing
        && directory.last().is_some_and(|p| {
            p.first_uncompressed_offset < tail.total_uncompressed
                && p.first_block_offset < tail.blocks_end
        });
    match valid {
        true => Ok(()),
        false => Err(BzstError::CorruptIndex),
    }
}

/// `value` as a little-endian `u32`, or an error naming the oversized `what`.
fn to_u32(value: u64, what: &'static str) -> BzstResult<[u8; 4]> {
    u32::try_from(value).map(u32::to_le_bytes).map_err(|_| BzstError::ExceedsFormatLimit(what))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, SeekFrom};

    use super::*;

    /// Where the sample index frames begin: after the header frame and blocks.
    const INDEX_OFFSET: u64 = 1 << 20;

    /// An index over `blocks` blocks of varying sizes starting right after the
    /// header frame, with a 40-byte derived frame before every `derived_every`th
    /// block (0 = none).
    fn sample_index(blocks: u64, derived_every: u64) -> Index {
        let mut builder = IndexBuilder::new();
        let mut offset = 24;
        for i in 0..blocks {
            if derived_every > 0 && i > 0 && i % derived_every == 0 {
                offset += 40;
            }
            let length = 100 + (i * 37) % 50;
            builder.push(offset, length, 1000 + (i * 13) % 100);
            offset += length;
        }
        builder.finish()
    }

    fn frame_of(index: &Index, partition_entries: usize) -> Vec<u8> {
        index.to_frame_bytes(INDEX_OFFSET, partition_entries).unwrap()
    }

    /// A whole "file" for the lazy reader: placeholder bytes up to the index
    /// frame (it never reads the blocks), then the frame.
    fn file_of(frame: &[u8]) -> Vec<u8> {
        let mut file = vec![0u8; INDEX_OFFSET as usize];
        file.extend_from_slice(frame);
        file
    }

    /// Offset within `frame` of the directory, from its partition count.
    fn directory_start(frame: &[u8]) -> usize {
        let count_at = frame.len() - TAIL_LEN + 24;
        let count = u32::from_le_bytes(frame[count_at..count_at + 4].try_into().unwrap());
        frame.len() - TAIL_LEN - count as usize * DIRECTORY_ENTRY_LEN
    }

    /// A partition for [`crafted_frame`]: `(first_uncompressed_offset,
    /// first_block_offset, sizes, derived_lengths, block_lengths)`.
    type CraftedPartition<'a> = (u64, u64, &'a [u32], &'a [u32], &'a [u32]);

    /// Builds an index frame directly from per-partition columns, so tests can
    /// craft contents the encoder would never produce.
    fn crafted_frame(partitions: &[CraftedPartition], total: u64) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&STRUCTURAL_MAGIC.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.push(SUBTYPE_INDEX);
        let mut zc = ZstdCompressor::new(DEFAULT_LEVEL, true).unwrap();
        let mut directory = Vec::new();
        let mut entry_count = 0u64;
        let mut blocks_end = HEADER_FRAME_LEN as u64;
        for &(first_uncompressed_offset, first_block_offset, sizes, derived, lengths) in partitions
        {
            let sum = |column: &[u32]| column.iter().map(|&v| u64::from(v)).sum::<u64>();
            blocks_end = first_block_offset + sum(lengths) + sum(&derived[1..]);
            let columns: Vec<u8> =
                [sizes, derived, lengths].concat().iter().flat_map(|v| v.to_le_bytes()).collect();
            let mut compressed = vec![0u8; ZstdCompressor::bound(columns.len())];
            let n = zc.compress(&columns, &mut compressed).unwrap();
            directory.push(Partition {
                first_uncompressed_offset,
                first_block_offset,
                offset: INDEX_OFFSET + f.len() as u64,
                length: n as u32,
                entry_count: sizes.len() as u32,
            });
            entry_count += sizes.len() as u64;
            f.extend_from_slice(&compressed[..n]);
        }
        let checked_start = f.len();
        directory.iter().for_each(|p| p.write_to(&mut f));
        f.extend_from_slice(&entry_count.to_le_bytes());
        f.extend_from_slice(&total.to_le_bytes());
        f.extend_from_slice(&blocks_end.to_le_bytes());
        f.extend_from_slice(&(directory.len() as u32).to_le_bytes());
        let checksum = crc32(&f[checked_start..]);
        f.extend_from_slice(&checksum.to_le_bytes());
        f.extend_from_slice(&INDEX_OFFSET.to_le_bytes());
        f.extend_from_slice(&EOF_MAGIC.to_le_bytes());
        let frame_size = (f.len() - 8) as u32;
        f[4..8].copy_from_slice(&frame_size.to_le_bytes());
        f
    }

    /// Overwrites the tail field `at` bytes into the tail with `bytes`, then
    /// recomputes the checksum so only the field's value is wrong.
    fn rewrite_tail_field(frame: &mut [u8], at: usize, bytes: &[u8]) {
        let tail_start = frame.len() - TAIL_LEN;
        frame[tail_start + at..tail_start + at + bytes.len()].copy_from_slice(bytes);
        let directory_at = directory_start(frame);
        let checksum = crc32(&frame[directory_at..tail_start + COUNTS_LEN]);
        frame[tail_start + COUNTS_LEN..tail_start + COUNTS_LEN + 4]
            .copy_from_slice(&checksum.to_le_bytes());
    }

    fn parse(frame: &[u8]) -> BzstResult<Index> {
        Index::parse_frame(frame, INDEX_OFFSET)
    }

    /// A reader that counts the bytes read through it.
    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: u64,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read += n as u64;
            Ok(n)
        }
    }

    impl Seek for CountingReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn multi_partition_index_round_trips() {
        // 1000 blocks at 64 per partition leaves a short final partition.
        let index = sample_index(1000, 0);
        assert_eq!(parse(&frame_of(&index, 64)).unwrap(), index);
    }

    #[test]
    fn derived_frames_between_blocks_round_trip() {
        let index = sample_index(300, 7);
        assert_eq!(parse(&frame_of(&index, 50)).unwrap(), index);
    }

    #[test]
    fn single_block_index_round_trips() {
        let index = sample_index(1, 0);
        assert_eq!(parse(&frame_of(&index, 4096)).unwrap(), index);
    }

    #[test]
    fn empty_index_round_trips() {
        let index = Index::default();
        assert_eq!(parse(&frame_of(&index, 4096)).unwrap(), index);
    }

    #[test]
    fn partition_size_does_not_change_the_decoded_index() {
        let index = sample_index(500, 9);
        for partition_entries in [1, 7, 128, 500, 4096] {
            assert_eq!(parse(&frame_of(&index, partition_entries)).unwrap(), index);
        }
    }

    #[test]
    fn checksum_covers_the_directory() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        // The second partition's First_Block_Offset.
        let second_first_block_offset = directory_start(&frame) + DIRECTORY_ENTRY_LEN + 8;
        frame[second_first_block_offset] ^= 0x01;
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn checksum_covers_the_total() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        let total_at = frame.len() - TAIL_LEN + 8;
        frame[total_at] ^= 0x01;
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn checksum_covers_blocks_end() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        let blocks_end_at = frame.len() - TAIL_LEN + 16;
        frame[blocks_end_at] ^= 0x01;
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn blocks_end_is_the_end_of_the_last_block() {
        let index = sample_index(300, 7);
        let last = index.entries().last().unwrap();
        assert_eq!(index.blocks_end(), last.block_offset + last.block_length);
        let lazy = LazyIndex::open(Cursor::new(file_of(&frame_of(&index, 64)))).unwrap();
        assert_eq!(lazy.blocks_end(), index.blocks_end());
        assert_eq!(lazy.index_offset(), INDEX_OFFSET);
    }

    #[test]
    fn empty_index_blocks_end_is_the_end_of_the_header() {
        let index = Index::default();
        assert_eq!(index.blocks_end(), HEADER_FRAME_LEN as u64);
        let lazy = LazyIndex::open(Cursor::new(file_of(&frame_of(&index, 64)))).unwrap();
        assert_eq!(lazy.blocks_end(), HEADER_FRAME_LEN as u64);
    }

    #[test]
    fn blocks_end_must_match_the_last_block() {
        let index = sample_index(300, 0);
        let mut frame = frame_of(&index, 64);
        rewrite_tail_field(&mut frame, 16, &(index.blocks_end() + 1).to_le_bytes());
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
        // A lazy reader catches it on reaching the last partition.
        let mut lazy = LazyIndex::open(Cursor::new(file_of(&frame))).unwrap();
        let last_offset = index.entries().last().unwrap().uncompressed_offset;
        assert!(matches!(lazy.entry_for_offset(last_offset), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn blocks_end_beyond_the_index_is_rejected() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        rewrite_tail_field(&mut frame, 16, &(INDEX_OFFSET + 1).to_le_bytes());
        let result = LazyIndex::open(Cursor::new(file_of(&frame)));
        assert!(matches!(result, Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn corrupt_partition_is_detected() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        // Inside the first partition's compressed body, past its zstd header.
        frame[HEAD_LEN + 20] ^= 0xFF;
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn frame_size_must_match_the_frame() {
        let mut frame = frame_of(&sample_index(10, 0), 64);
        frame[4] ^= 0x01;
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn index_must_be_where_its_trailer_says() {
        let frame = frame_of(&sample_index(10, 0), 64);
        assert!(matches!(
            Index::parse_frame(&frame, INDEX_OFFSET + 1),
            Err(BzstError::CorruptIndex)
        ));
    }

    #[test]
    fn crafted_partition_count_errors_not_panics() {
        let mut frame = frame_of(&Index::default(), 64);
        let count_at = frame.len() - TAIL_LEN + 24;
        frame[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn crafted_frame_matching_the_encoder_parses() {
        // Guards the crafted-frame tests below against passing for the wrong reason.
        let frame = crafted_frame(&[(0, 24, &[10, 20], &[0, 5], &[30, 40])], 30);
        let index = parse(&frame).unwrap();
        assert_eq!(
            index.entries(),
            [
                IndexEntry { uncompressed_offset: 0, block_offset: 24, block_length: 30 },
                IndexEntry { uncompressed_offset: 10, block_offset: 59, block_length: 40 },
            ]
        );
    }

    #[test]
    fn derived_length_on_a_partitions_first_entry_is_rejected() {
        let frame = crafted_frame(&[(0, 24, &[10, 20], &[5, 0], &[30, 40])], 30);
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn zero_uncompressed_size_is_rejected() {
        let frame = crafted_frame(&[(0, 24, &[10, 0, 20], &[0, 0, 0], &[30, 30, 30])], 30);
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn partition_sizes_must_reach_the_next_partition() {
        // The first partition's sizes sum to 10, but the second claims to start at 15.
        let frame = crafted_frame(&[(0, 24, &[10], &[0], &[30]), (15, 54, &[20], &[0], &[30])], 35);
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn partition_sizes_must_reach_the_total() {
        let frame = crafted_frame(&[(0, 24, &[10, 20], &[0, 0], &[30, 40])], 31);
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn blocks_overlapping_the_next_partition_are_rejected() {
        // The first partition's block ends at 24 + 30 = 54, after the second's start.
        let frame = crafted_frame(&[(0, 24, &[10], &[0], &[30]), (10, 50, &[20], &[0], &[30])], 30);
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn partition_without_content_checksum_is_rejected() {
        let index = sample_index(10, 0);
        let mut frame = frame_of(&index, 64);
        // Rebuild the lone partition without a content checksum, keeping the length
        // and checksum fields consistent so only the missing flag is wrong.
        let columns = index.partition_columns(0, 10).unwrap();
        let mut zc = ZstdCompressor::new(DEFAULT_LEVEL, false).unwrap();
        let mut compressed = vec![0u8; ZstdCompressor::bound(columns.len())];
        let n = zc.compress(&columns, &mut compressed).unwrap();
        let tail_and_directory = frame.split_off(directory_start(&frame));
        frame.truncate(HEAD_LEN);
        frame.extend_from_slice(&compressed[..n]);
        frame.extend_from_slice(&tail_and_directory);
        let directory_at = directory_start(&frame);
        frame[directory_at + 24..directory_at + 28].copy_from_slice(&(n as u32).to_le_bytes());
        let checksum = crc32(&frame[directory_at..frame.len() - TAIL_LEN + COUNTS_LEN]);
        let checksum_at = frame.len() - TAIL_LEN + COUNTS_LEN;
        frame[checksum_at..checksum_at + 4].copy_from_slice(&checksum.to_le_bytes());
        let frame_size = (frame.len() - 8) as u32;
        frame[4..8].copy_from_slice(&frame_size.to_le_bytes());
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn forged_entry_count_is_rejected_before_allocating() {
        // A directory claiming u32::MAX entries in a partition that holds one.
        let mut frame = crafted_frame(&[(0, 24, &[10], &[0], &[30])], 10);
        let directory_at = directory_start(&frame);
        frame[directory_at + 28..directory_at + 32].copy_from_slice(&u32::MAX.to_le_bytes());
        rewrite_tail_field(&mut frame, 0, &u64::from(u32::MAX).to_le_bytes());
        assert!(matches!(parse(&frame), Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn encoder_rejects_decreasing_offsets() {
        let index = Index {
            entries: vec![
                IndexEntry { uncompressed_offset: 100, block_offset: 24, block_length: 50 },
                IndexEntry { uncompressed_offset: 50, block_offset: 74, block_length: 50 },
            ],
            total_uncompressed: 200,
        };
        assert!(matches!(index.to_frame_bytes(0, 64), Err(BzstError::Malformed(_))));
    }

    #[test]
    fn encoder_rejects_frames_between_blocks_beyond_u32() {
        let index = Index {
            entries: vec![
                IndexEntry { uncompressed_offset: 0, block_offset: 24, block_length: 50 },
                IndexEntry {
                    uncompressed_offset: 10,
                    block_offset: 74 + (1 << 32),
                    block_length: 50,
                },
            ],
            total_uncompressed: 20,
        };
        assert!(matches!(
            index.to_frame_bytes(0, 64),
            Err(BzstError::ExceedsFormatLimit("the frames between two blocks"))
        ));
    }

    #[test]
    fn lazy_index_agrees_with_the_full_index() {
        let index = sample_index(1000, 11);
        let mut lazy = LazyIndex::open(Cursor::new(file_of(&frame_of(&index, 64)))).unwrap();
        assert_eq!(lazy.len(), 1000);
        assert_eq!(lazy.partition_count(), 16);
        assert_eq!(lazy.total_uncompressed(), index.total_uncompressed());
        for offset in (0..index.total_uncompressed()).step_by(97) {
            let expected = index.block_for_offset(offset).map(|i| index.entries()[i]);
            assert_eq!(lazy.entry_for_offset(offset).unwrap(), expected, "offset {offset}");
        }
        assert_eq!(lazy.entry_for_offset(index.total_uncompressed()).unwrap(), None);
    }

    #[test]
    fn lazy_index_reads_only_the_tail_then_one_partition_per_lookup() {
        let index = sample_index(2000, 0);
        let frame = frame_of(&index, 100);
        let reader = CountingReader { inner: Cursor::new(file_of(&frame)), bytes_read: 0 };
        // A tail read too short for the 20-entry directory forces the second read.
        let mut lazy = LazyIndex::open_with_tail_read(reader, 256).unwrap();
        let directory_and_tail = 20 * DIRECTORY_ENTRY_LEN + TAIL_LEN;
        assert_eq!(lazy.inner.bytes_read, directory_and_tail as u64);

        // Each lookup in a new partition reads exactly that partition.
        for p in [0, 7, 19] {
            let before = lazy.inner.bytes_read;
            let offset = lazy.directory[p].first_uncompressed_offset;
            lazy.entry_for_offset(offset).unwrap();
            assert_eq!(lazy.inner.bytes_read - before, u64::from(lazy.directory[p].length));
        }
        // A second lookup in the cached partition reads nothing.
        let before = lazy.inner.bytes_read;
        let offset = lazy.directory[19].first_uncompressed_offset + 1;
        lazy.entry_for_offset(offset).unwrap();
        assert_eq!(lazy.inner.bytes_read, before);
        assert!(lazy.inner.bytes_read < frame.len() as u64);
    }

    #[test]
    fn lazy_index_of_a_truncated_file_is_truncated() {
        let mut file = file_of(&frame_of(&sample_index(10, 0), 64));
        file.pop();
        assert!(matches!(LazyIndex::open(Cursor::new(file)), Err(BzstError::Truncated)));
    }

    #[test]
    fn lazy_index_detects_a_corrupt_directory() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        let first_first_block_offset = directory_start(&frame) + 8;
        frame[first_first_block_offset] ^= 0x01;
        let result = LazyIndex::open(Cursor::new(file_of(&frame)));
        assert!(matches!(result, Err(BzstError::CorruptIndex)));
    }

    #[test]
    fn lazy_index_detects_a_corrupt_partition_on_lookup() {
        let mut frame = frame_of(&sample_index(300, 0), 64);
        frame[HEAD_LEN + 20] ^= 0xFF;
        let mut lazy = LazyIndex::open(Cursor::new(file_of(&frame))).unwrap();
        assert!(matches!(lazy.entry_for_offset(0), Err(BzstError::CorruptIndex)));
    }
}
