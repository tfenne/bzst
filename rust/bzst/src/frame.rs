//! The bzst wire format: structural frames, typed views, and low-level frame
//! I/O. All of the on-disk byte layout lives here.
//!
//! [`FrameReader`]/[`FrameWriter`]/[`EncodedBlock`] are `pub(crate)`: the public
//! surface is the high-level reader/writer plus the operations, not raw frame
//! poking. [`Header`], [`BlockHeader`], [`Frame`], etc. are public typed views.

use std::io::{self, Read, Write};

use crate::codec::ZstdCompressor;
use crate::index::Index;
use crate::memory::{check_block_fits, default_alloc_limit};
use crate::{
    xxh64, BzstError, BzstResult, FORMAT_VERSION, SIGNATURE, SKIPPABLE_MAGIC_MAX,
    SKIPPABLE_MAGIC_MIN, STRUCTURAL_MAGIC, SUBTYPE_BLOCK_HEADER, SUBTYPE_HEADER, SUBTYPE_INDEX,
    ZSTD_FRAME_MAGIC,
};

/// On-disk length of a header frame.
pub(crate) const HEADER_FRAME_LEN: usize = 28;
/// On-disk length of a block-header frame.
pub(crate) const BLOCK_HEADER_FRAME_LEN: usize = 30;
/// Length of a skippable-frame envelope (magic + u32 size).
const SKIPPABLE_HEADER_LEN: usize = 8;

/// Writes bzst frames to an underlying `Write`, tracking the byte position so
/// block offsets and the trailing index offset can be recorded. Pure framing —
/// it never compresses; it writes already-[`EncodedBlock`]s.
pub(crate) struct FrameWriter<W> {
    inner: W,
    pos: u64,
}

impl<W: Write> FrameWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner, pos: 0 }
    }

    pub(crate) fn write_header(&mut self, header: &Header) -> BzstResult<()> {
        let bytes = header.to_frame_bytes();
        self.inner.write_all(&bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Writes `[block-header frame][data frame]`, returning the offset at which
    /// the block-header frame starts.
    pub(crate) fn write_encoded_block(&mut self, block: &EncodedBlock) -> BzstResult<u64> {
        let offset = self.pos;
        let bh = block.header.to_frame_bytes();
        self.inner.write_all(&bh)?;
        self.inner.write_all(&block.data)?;
        self.pos += bh.len() as u64 + block.data.len() as u64;
        Ok(offset)
    }

    pub(crate) fn write_skippable(&mut self, magic: u32, payload: &[u8]) -> BzstResult<()> {
        let size: u32 = payload
            .len()
            .try_into()
            .map_err(|_| BzstError::Malformed("skippable frame payload exceeds the 4 GiB limit"))?;
        self.inner.write_all(&magic.to_le_bytes())?;
        self.inner.write_all(&size.to_le_bytes())?;
        self.inner.write_all(payload)?;
        self.pos += SKIPPABLE_HEADER_LEN as u64 + payload.len() as u64;
        Ok(())
    }

    pub(crate) fn write_index(&mut self, index: &Index) -> BzstResult<()> {
        let bytes = index.to_frame_bytes(self.pos)?;
        self.inner.write_all(&bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> BzstResult<()> {
        self.inner.flush()?;
        Ok(())
    }

    pub(crate) fn into_inner(self) -> W {
        self.inner
    }
}

/// Reads bzst frames from an underlying `Read`, one at a time. A lending
/// iterator: the returned [`Frame`] borrows an internal buffer, so only one
/// frame is live at a time.
pub(crate) struct FrameReader<R> {
    inner: R,
    frame: Vec<u8>,
    data: Vec<u8>,
    pos: u64,
    /// Ceiling on a single block's compressed + uncompressed bytes, guarding
    /// against a corrupt size field driving an out-of-memory allocation.
    max_block_bytes: u64,
}

impl<R: Read> FrameReader<R> {
    pub(crate) fn new(inner: R, max_block_bytes: u64) -> Self {
        Self { inner, frame: Vec::new(), data: Vec::new(), pos: 0, max_block_bytes }
    }

    /// Byte offset of the next frame to be read (== bytes consumed so far).
    pub(crate) fn position(&self) -> u64 {
        self.pos
    }

    pub(crate) fn next_frame(&mut self) -> BzstResult<Option<Frame<'_>>> {
        let mut magic_buf = [0u8; 4];
        match read_full(&mut self.inner, &mut magic_buf)? {
            0 => return Ok(None),
            4 => {}
            _ => return Err(BzstError::Truncated),
        }
        let magic = u32::from_le_bytes(magic_buf);

        if magic == STRUCTURAL_MAGIC {
            let mut size_buf = [0u8; 4];
            read_exact_or_trunc(&mut self.inner, &mut size_buf)?;
            let size = u32::from_le_bytes(size_buf) as usize;
            // Bound the frame-envelope allocation the same way as block data: a
            // corrupt 32-bit size field must not drive a multi-GiB allocation.
            check_block_fits(size as u64, 0, self.max_block_bytes)?;
            self.frame.clear();
            self.frame.extend_from_slice(&magic_buf);
            self.frame.extend_from_slice(&size_buf);
            let body = self.frame.len();
            self.frame.resize(body + size, 0);
            read_exact_or_trunc(&mut self.inner, &mut self.frame[body..])?;
            self.pos += (SKIPPABLE_HEADER_LEN + size) as u64;
            if self.frame.len() < 9 {
                return Err(BzstError::Truncated);
            }
            match self.frame[8] {
                SUBTYPE_HEADER => Ok(Some(Frame::Header(Header::parse_frame(&self.frame)?))),
                SUBTYPE_BLOCK_HEADER => {
                    let header = BlockHeader::parse_frame(&self.frame)?;
                    check_block_fits(
                        header.compressed_size,
                        header.uncompressed_size,
                        self.max_block_bytes,
                    )?;
                    self.data.clear();
                    self.data.resize(header.compressed_size as usize, 0);
                    read_exact_or_trunc(&mut self.inner, &mut self.data)?;
                    self.pos += header.compressed_size;
                    Ok(Some(Frame::Block { header, data: &self.data }))
                }
                // Recognize the index frame WITHOUT validating its body: a corrupt
                // trailing index must not break the forward read path, since the
                // block-header frames are the source of truth. Callers that need
                // the parsed index (`Frames`, `Index::read_from`) validate it.
                SUBTYPE_INDEX => Ok(Some(Frame::Index(&self.frame))),
                // Unknown structural subtype (a future version): surface it so
                // callers can skip it; the `Read` path ignores non-Block frames.
                _ => Ok(Some(Frame::Skippable(SkippableFrame {
                    magic: STRUCTURAL_MAGIC,
                    payload: &self.frame[8..],
                }))),
            }
        } else if (SKIPPABLE_MAGIC_MIN..=SKIPPABLE_MAGIC_MAX).contains(&magic) {
            let mut size_buf = [0u8; 4];
            read_exact_or_trunc(&mut self.inner, &mut size_buf)?;
            let size = u32::from_le_bytes(size_buf) as usize;
            check_block_fits(size as u64, 0, self.max_block_bytes)?;
            self.frame.clear();
            self.frame.resize(size, 0);
            read_exact_or_trunc(&mut self.inner, &mut self.frame)?;
            self.pos += (SKIPPABLE_HEADER_LEN + size) as u64;
            Ok(Some(Frame::Skippable(SkippableFrame { magic, payload: &self.frame })))
        } else if magic == ZSTD_FRAME_MAGIC {
            // A data frame must be preceded by a block-header frame in bzst.
            Err(BzstError::Malformed("data frame not preceded by a block header"))
        } else {
            Err(BzstError::BadMagic { expected: STRUCTURAL_MAGIC, found: magic })
        }
    }
}

/// A compressed block ready to be written: the typed header plus the zstd data
/// frame. This is the unit the parallel pipeline moves between threads.
pub(crate) struct EncodedBlock {
    pub(crate) header: BlockHeader,
    pub(crate) data: Vec<u8>,
}

impl EncodedBlock {
    /// Compresses `uncompressed` into one zstd data frame using `zc`.
    pub(crate) fn encode(zc: &mut ZstdCompressor, uncompressed: &[u8]) -> BzstResult<Self> {
        let mut data = vec![0u8; ZstdCompressor::bound(uncompressed.len())];
        let n = zc.compress(uncompressed, &mut data)?;
        data.truncate(n);
        Ok(Self {
            header: BlockHeader {
                compressed_size: n as u64,
                uncompressed_size: uncompressed.len() as u64,
                flags: BlockFlags::default(),
            },
            data,
        })
    }

    /// On-disk length of the block (`[block-header frame][data frame]`).
    pub(crate) fn on_disk_len(&self) -> u64 {
        block_on_disk_len(self.data.len() as u64)
    }
}

/// The file header frame (subtype `0x00`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// bzst format version.
    pub format_version: u8,
    /// Opaque 4-byte derived-format tag; `[0; 4]` means a generic bzst file.
    pub format_signature: [u8; 4],
    /// Set of payload profiles the file uses.
    pub profiles: Profiles,
}

impl Header {
    pub(crate) fn new(format_signature: [u8; 4], profiles: Profiles) -> Self {
        Self { format_version: FORMAT_VERSION, format_signature, profiles }
    }

    pub(crate) fn read_from<R: Read>(r: &mut R) -> BzstResult<Self> {
        let mut buf = [0u8; HEADER_FRAME_LEN];
        read_exact_or_trunc(r, &mut buf)?;
        Self::parse_frame(&buf)
    }

    fn to_frame_bytes(&self) -> [u8; HEADER_FRAME_LEN] {
        let mut f = [0u8; HEADER_FRAME_LEN];
        f[0..4].copy_from_slice(&STRUCTURAL_MAGIC.to_le_bytes());
        f[4..8].copy_from_slice(&((HEADER_FRAME_LEN - 8) as u32).to_le_bytes());
        f[8] = SUBTYPE_HEADER;
        f[9] = self.format_version;
        f[10..14].copy_from_slice(&SIGNATURE);
        f[14..18].copy_from_slice(&self.format_signature);
        f[18] = self.profiles.bits();
        f[19] = 0; // reserved flags
        let cksum = xxh64(&f[0..20]);
        f[20..28].copy_from_slice(&cksum.to_le_bytes());
        f
    }

    fn parse_frame(f: &[u8]) -> BzstResult<Self> {
        if f.len() < HEADER_FRAME_LEN {
            return Err(BzstError::Truncated);
        }
        let magic = u32::from_le_bytes(f[0..4].try_into().unwrap());
        if magic != STRUCTURAL_MAGIC {
            return Err(BzstError::BadMagic { expected: STRUCTURAL_MAGIC, found: magic });
        }
        let sig: [u8; 4] = f[10..14].try_into().unwrap();
        if f[8] != SUBTYPE_HEADER || sig != SIGNATURE {
            return Err(BzstError::BadMagic {
                expected: u32::from_le_bytes(SIGNATURE),
                found: u32::from_le_bytes(sig),
            });
        }
        let stored = u64::from_le_bytes(f[20..28].try_into().unwrap());
        if xxh64(&f[0..20]) != stored {
            return Err(BzstError::ChecksumMismatch { frame: "header" });
        }
        let version = f[9];
        if version != FORMAT_VERSION {
            return Err(BzstError::UnsupportedVersion(version));
        }
        Ok(Self {
            format_version: version,
            format_signature: f[14..18].try_into().unwrap(),
            profiles: Profiles::from_bits(f[18]),
        })
    }
}

/// A per-block header frame (subtype `0x01`): the sizes needed to place and
/// decode the following data frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// Exact on-disk size of the following data frame.
    pub compressed_size: u64,
    /// Size of the following data frame's decoded output.
    pub uncompressed_size: u64,
    /// Advisory per-block flags.
    pub flags: BlockFlags,
}

impl BlockHeader {
    fn to_frame_bytes(self) -> [u8; BLOCK_HEADER_FRAME_LEN] {
        let mut f = [0u8; BLOCK_HEADER_FRAME_LEN];
        f[0..4].copy_from_slice(&STRUCTURAL_MAGIC.to_le_bytes());
        f[4..8].copy_from_slice(&((BLOCK_HEADER_FRAME_LEN - 8) as u32).to_le_bytes());
        f[8] = SUBTYPE_BLOCK_HEADER;
        f[9..17].copy_from_slice(&self.compressed_size.to_le_bytes());
        f[17..25].copy_from_slice(&self.uncompressed_size.to_le_bytes());
        f[25] = self.flags.bits();
        let cksum = xxh64(&f[0..26]) as u32;
        f[26..30].copy_from_slice(&cksum.to_le_bytes());
        f
    }

    pub(crate) fn parse_frame(f: &[u8]) -> BzstResult<Self> {
        if f.len() < BLOCK_HEADER_FRAME_LEN {
            return Err(BzstError::Truncated);
        }
        let stored = u32::from_le_bytes(f[26..30].try_into().unwrap());
        if xxh64(&f[0..26]) as u32 != stored {
            return Err(BzstError::ChecksumMismatch { frame: "block-header" });
        }
        Ok(Self {
            compressed_size: u64::from_le_bytes(f[9..17].try_into().unwrap()),
            uncompressed_size: u64::from_le_bytes(f[17..25].try_into().unwrap()),
            flags: BlockFlags::from_bits(f[25]),
        })
    }
}

/// The set of payload profiles a file uses (a header bitmask). In v1 only the
/// baseline profile is defined; the dictionary bit is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profiles(u8);

impl Profiles {
    /// The baseline profile: plain zstd data frames, no dictionary.
    pub const BASELINE: Profiles = Profiles(0);
    const DICTIONARY_BIT: u8 = 0b0000_0001;

    /// True if the file uses only the baseline profile.
    pub fn is_baseline(self) -> bool {
        self.0 == 0
    }

    /// True if any block is dictionary-compressed (reserved; always false in v1 output).
    pub fn uses_dictionary(self) -> bool {
        self.0 & Self::DICTIONARY_BIT != 0
    }

    fn bits(self) -> u8 {
        self.0
    }

    fn from_bits(b: u8) -> Self {
        Profiles(b)
    }
}

/// Advisory per-block flags (bit 0 = `Stored`: the data frame is uncompressed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockFlags(u8);

impl BlockFlags {
    const STORED_BIT: u8 = 0b0000_0001;

    /// Advisory hint that the data frame is stored (uncompressed); the zstd
    /// framing is authoritative.
    pub fn stored(self) -> bool {
        self.0 & Self::STORED_BIT != 0
    }

    fn bits(self) -> u8 {
        self.0
    }

    fn from_bits(b: u8) -> Self {
        BlockFlags(b)
    }
}

// A borrowed, read-side view of a skippable frame (internal; the public,
// non-lending view is `OwnedFrame`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkippableFrame<'a> {
    pub(crate) magic: u32,
    pub(crate) payload: &'a [u8],
}

// A single borrowed frame (internal, lending). The public walking API is
// `Frames`/`OwnedFrame`.
#[derive(Debug)]
pub(crate) enum Frame<'a> {
    Header(Header),
    Block {
        header: BlockHeader,
        data: &'a [u8],
    },
    Skippable(SkippableFrame<'a>),
    /// The raw bytes of the trailing index frame, parsed on demand — a corrupt
    /// index must not fail the forward read path.
    Index(&'a [u8]),
}

impl Frame<'_> {
    fn to_owned_frame(&self) -> BzstResult<OwnedFrame> {
        Ok(match self {
            Frame::Header(h) => OwnedFrame::Header(h.clone()),
            Frame::Block { header, .. } => OwnedFrame::Block(*header),
            Frame::Skippable(s) => {
                OwnedFrame::Skippable { magic: s.magic, payload: s.payload.to_vec() }
            }
            Frame::Index(raw) => OwnedFrame::Index(Index::parse_frame(raw)?),
        })
    }
}

/// An owned frame yielded while walking a bzst stream with [`Frames`]. Block
/// payload is *not* copied — only the header — so scanning structure (or a
/// derived format reading its skippable metadata) is cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedFrame {
    /// The file header.
    Header(Header),
    /// A data block's header (sizes + flags). The compressed data is not included.
    Block(BlockHeader),
    /// A skippable frame: its magic and a copy of its payload.
    Skippable {
        /// The frame's magic number.
        magic: u32,
        /// A copy of the frame's payload.
        payload: Vec<u8>,
    },
    /// The index frame.
    Index(Index),
}

/// An iterator over every frame in a bzst stream, in order. Useful for `bzst
/// inspect` and for derived formats reading their own skippable metadata frames.
pub struct Frames<R> {
    fr: FrameReader<R>,
}

impl<R: Read> Frames<R> {
    /// Walks the frames of `r` from the current position.
    pub fn new(r: R) -> Self {
        Self { fr: FrameReader::new(r, default_alloc_limit()) }
    }
}

impl<R: Read> Iterator for Frames<R> {
    type Item = BzstResult<OwnedFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.fr.next_frame() {
            Ok(None) => None,
            Ok(Some(frame)) => Some(frame.to_owned_frame()),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Reads up to `buf.len()` bytes, returning how many were read. Returns fewer
/// only at end of input, letting the caller distinguish a clean EOF (0) from a
/// truncated frame.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Like `read_exact`, but reports a short read (mid-frame EOF) as
/// [`BzstError::Truncated`] rather than a bare io error, so truncation is
/// diagnosed consistently across the frame layer.
fn read_exact_or_trunc<R: Read>(r: &mut R, buf: &mut [u8]) -> BzstResult<()> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(BzstError::Truncated),
        Err(e) => Err(BzstError::Io(e)),
    }
}

/// On-disk length of a block (`[block-header frame][data frame]`) given the data
/// frame's compressed size. Shared by the writer and by [`Index::rebuild`].
pub(crate) fn block_on_disk_len(compressed_size: u64) -> u64 {
    BLOCK_HEADER_FRAME_LEN as u64 + compressed_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BzstError;

    #[test]
    fn header_checksum_flip_is_detected() {
        let mut bytes = Header::new([0; 4], Profiles::BASELINE).to_frame_bytes();
        bytes[19] ^= 0xFF; // reserved byte, inside the checksummed region
        assert!(matches!(
            Header::parse_frame(&bytes),
            Err(BzstError::ChecksumMismatch { frame: "header" })
        ));
    }

    #[test]
    fn block_header_checksum_flip_is_detected() {
        let bh = BlockHeader {
            compressed_size: 100,
            uncompressed_size: 200,
            flags: BlockFlags::default(),
        };
        let mut bytes = bh.to_frame_bytes();
        bytes[10] ^= 0xFF; // inside compressed_size, within the checksummed region
        assert!(matches!(
            BlockHeader::parse_frame(&bytes),
            Err(BzstError::ChecksumMismatch { frame: "block-header" })
        ));
    }

    #[test]
    fn oversized_uncompressed_size_is_rejected_before_allocating() {
        // A block header with a valid checksum but a forged, huge uncompressed_size
        // and a tiny compressed frame must be rejected on the uncompressed side —
        // compressed alone is well under the cap — proving both sizes are checked
        // before the decode buffer would be allocated.
        let header = BlockHeader {
            compressed_size: 8,
            uncompressed_size: 1 << 40,
            flags: BlockFlags::default(),
        };
        let mut bytes = header.to_frame_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 8]); // placeholder for the (unread) data frame
        let mut fr = FrameReader::new(std::io::Cursor::new(bytes), 1 << 20); // 1 MiB cap
        assert!(matches!(fr.next_frame(), Err(BzstError::BlockTooLarge { .. })));
    }
}
