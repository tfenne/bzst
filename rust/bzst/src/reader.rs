//! Reading bzst streams: [`BzstReader`] (forward, serial or parallel read-ahead)
//! and [`SeekableReader`] (random access via the index).

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};

use rayon::prelude::*;

use crate::codec::ZstdDecompressor;
use crate::frame::{BlockHeader, Frame, FrameReader, BLOCK_HEADER_FRAME_LEN};
use crate::index::Index;
use crate::writer::{Pool, Threads};
use crate::{BzstError, BzstResult, Header};

/// Forward reader over a bzst stream. Implements [`std::io::Read`], yielding the
/// concatenated decompressed payload; decompression is serial or, with
/// [`Threads`], batched read-ahead in parallel.
pub struct BzstReader<R> {
    fr: FrameReader<R>,
    header: Header,
    strategy: ReadStrategy,
    queue: VecDeque<Vec<u8>>,
    cur: Vec<u8>,
    cur_pos: usize,
    done: bool,
}

impl<R: Read> BzstReader<R> {
    /// Creates a reader, validating and consuming the header frame.
    pub fn new(r: R) -> BzstResult<Self> {
        Self::builder(r).build()
    }

    /// Starts a [`BzstReaderBuilder`] to configure threading.
    pub fn builder(r: R) -> BzstReaderBuilder<R> {
        BzstReaderBuilder::new(r)
    }

    /// The file header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    fn refill(&mut self) -> BzstResult<()> {
        let batch = match &self.strategy {
            ReadStrategy::Serial(_) => 1,
            ReadStrategy::Parallel { batch, .. } => *batch,
        };
        let raw = self.read_raw_blocks(batch)?;
        if raw.is_empty() {
            return Ok(());
        }
        let decoded: Vec<Vec<u8>> = match &mut self.strategy {
            ReadStrategy::Serial(dec) => {
                let mut out = Vec::with_capacity(raw.len());
                for (header, data) in &raw {
                    out.push(decode_block(dec, header, data)?);
                }
                out
            }
            ReadStrategy::Parallel { pool, .. } => {
                let pool = pool.clone();
                pool.0.install(|| {
                    raw.par_iter()
                        .map_init(
                            || ZstdDecompressor::new().expect("decompressor"),
                            |dec, (header, data)| decode_block(dec, header, data),
                        )
                        .collect::<BzstResult<Vec<Vec<u8>>>>()
                })?
            }
        };
        self.queue.extend(decoded);
        Ok(())
    }

    fn read_raw_blocks(&mut self, max: usize) -> BzstResult<Vec<(BlockHeader, Vec<u8>)>> {
        let mut out = Vec::new();
        while out.len() < max {
            match self.fr.next_frame()? {
                None => break,
                Some(Frame::Block { header, data }) => out.push((header, data.to_vec())),
                Some(_) => {} // header/skippable/index: not payload, skip
            }
        }
        Ok(out)
    }
}

impl<R: Read> Read for BzstReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cur_pos < self.cur.len() {
                let n = buf.len().min(self.cur.len() - self.cur_pos);
                buf[..n].copy_from_slice(&self.cur[self.cur_pos..self.cur_pos + n]);
                self.cur_pos += n;
                return Ok(n);
            }
            if let Some(next) = self.queue.pop_front() {
                self.cur = next;
                self.cur_pos = 0;
                continue;
            }
            if self.done {
                return Ok(0);
            }
            self.refill()?;
            if self.queue.is_empty() {
                self.done = true;
                return Ok(0);
            }
        }
    }
}

/// Builder for [`BzstReader`].
pub struct BzstReaderBuilder<R> {
    inner: R,
    threads: Threads,
    batch: usize,
}

impl<R: Read> BzstReaderBuilder<R> {
    fn new(inner: R) -> Self {
        Self { inner, threads: Threads::Serial, batch: 0 }
    }

    /// Threading strategy for read-ahead decompression (default [`Threads::Serial`]).
    pub fn threads(mut self, threads: Threads) -> Self {
        self.threads = threads;
        self
    }

    /// Number of blocks to read ahead and decompress per batch in parallel mode.
    pub fn batch_size(mut self, n: usize) -> Self {
        self.batch = n;
        self
    }

    /// Builds the reader, validating and consuming the header frame.
    pub fn build(self) -> BzstResult<BzstReader<R>> {
        let BzstReaderBuilder { inner, threads, batch } = self;
        let mut fr = FrameReader::new(inner);
        let header = match fr.next_frame()? {
            Some(Frame::Header(h)) => h,
            _ => return Err(BzstError::Truncated),
        };
        let batch_of = |t: usize| if batch > 0 { batch } else { (t * 4).max(8) };
        let strategy = match threads {
            Threads::Serial => ReadStrategy::Serial(ZstdDecompressor::new()?),
            Threads::Owned(n) => {
                let pool = Pool::new(n)?;
                let b = batch_of(pool.0.current_num_threads());
                ReadStrategy::Parallel { pool, batch: b }
            }
            Threads::Shared(pool) => {
                let b = batch_of(pool.0.current_num_threads());
                ReadStrategy::Parallel { pool, batch: b }
            }
        };
        Ok(BzstReader {
            fr,
            header,
            strategy,
            queue: VecDeque::new(),
            cur: Vec::new(),
            cur_pos: 0,
            done: false,
        })
    }
}

/// Random-access reader over a bzst file's uncompressed content. Implements
/// [`std::io::Read`] + [`std::io::Seek`] over the *uncompressed* stream, backed
/// by the file's index.
pub struct SeekableReader<R> {
    inner: R,
    index: Index,
    header: Header,
    dec: ZstdDecompressor,
    cached_block: Option<usize>,
    cache: Vec<u8>,
    pos: u64,
}

impl<R: Read + Seek> SeekableReader<R> {
    /// Opens a seekable reader, loading the header and index.
    pub fn new(mut inner: R) -> BzstResult<Self> {
        inner.seek(SeekFrom::Start(0))?;
        let header = Header::read_from(&mut inner)?;
        let index = Index::read_from(&mut inner)?;
        Ok(Self {
            inner,
            index,
            header,
            dec: ZstdDecompressor::new()?,
            cached_block: None,
            cache: Vec::new(),
            pos: 0,
        })
    }

    /// The file header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The file's index.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Total uncompressed size of the file.
    pub fn total_uncompressed(&self) -> u64 {
        self.index.total_uncompressed()
    }

    /// Reads uncompressed bytes starting at `offset` into `buf`, returning how
    /// many were read (0 at or past the end).
    pub fn read_range(&mut self, offset: u64, buf: &mut [u8]) -> BzstResult<usize> {
        let mut written = 0;
        let mut pos = offset;
        while written < buf.len() {
            let Some(bi) = self.index.block_for_offset(pos) else { break };
            self.ensure_block(bi)?;
            let entry = self.index.entry(bi).ok_or(BzstError::Truncated)?;
            let within = (pos - entry.uncompressed_offset) as usize;
            if within >= self.cache.len() {
                break;
            }
            let n = (self.cache.len() - within).min(buf.len() - written);
            buf[written..written + n].copy_from_slice(&self.cache[within..within + n]);
            written += n;
            pos += n as u64;
        }
        Ok(written)
    }

    fn ensure_block(&mut self, bi: usize) -> BzstResult<()> {
        if self.cached_block == Some(bi) {
            return Ok(());
        }
        let entry = *self.index.entry(bi).ok_or(BzstError::Truncated)?;
        self.inner.seek(SeekFrom::Start(entry.block_offset))?;
        let mut block = vec![0u8; entry.block_length as usize];
        self.inner.read_exact(&mut block)?;
        let bh = BlockHeader::parse_frame(&block)?;
        let data = &block[BLOCK_HEADER_FRAME_LEN..];
        self.cache.clear();
        self.cache.resize(bh.uncompressed_size as usize, 0);
        let n = self.dec.decompress(data, &mut self.cache)?;
        self.cache.truncate(n);
        self.cached_block = Some(bi);
        Ok(())
    }
}

impl<R: Read + Seek> Read for SeekableReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_range(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for SeekableReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let total = self.index.total_uncompressed();
        let new = match pos {
            SeekFrom::Start(o) => o,
            SeekFrom::End(o) => total.saturating_add_signed(o),
            SeekFrom::Current(o) => self.pos.saturating_add_signed(o),
        };
        self.pos = new;
        Ok(new)
    }
}

// Internal per-reader decompression strategy.
enum ReadStrategy {
    Serial(ZstdDecompressor),
    Parallel { pool: Pool, batch: usize },
}

/// Decompresses one block's data frame using its header for the exact size.
fn decode_block(
    dec: &mut ZstdDecompressor,
    header: &BlockHeader,
    data: &[u8],
) -> BzstResult<Vec<u8>> {
    let mut out = vec![0u8; header.uncompressed_size as usize];
    let n = dec.decompress(data, &mut out)?;
    out.truncate(n);
    Ok(out)
}
