//! Reading bzst streams: [`BzstReader`] (forward, serial or parallel read-ahead)
//! and [`SeekableReader`] (random access via the index).
//!
//! The parallel forward path mirrors the writer: it is a pipeline, not a batch
//! barrier. The calling thread reads compressed blocks off the stream and
//! dispatches each to a worker [`Pool`] paired with a one-shot channel, holding
//! the receiving ends in a bounded in-flight window (which supplies
//! back-pressure). Workers decompress later blocks while the consumer drains
//! earlier ones in order, so decode and I/O overlap.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::mpsc::{self, Receiver};

use crate::codec::ZstdDecompressor;
use crate::frame::{BlockHeader, Frame, FrameReader, BLOCK_HEADER_FRAME_LEN};
use crate::index::Index;
use crate::memory::{check_block_fits, default_alloc_limit};
use crate::threads::{max_blocks_for_threads, Pool, Threads};
use crate::{BzstError, BzstResult, Header};

/// Forward reader over a bzst stream. Implements [`std::io::Read`], yielding the
/// concatenated decompressed payload; decompression is serial or, with
/// [`Threads`], batched read-ahead in parallel.
pub struct BzstReader<R> {
    fr: FrameReader<R>,
    header: Header,
    strategy: ReadStrategy,
    cur: Vec<u8>,
    cur_pos: usize,
    /// Whether the trailing index frame has been seen; its absence at EOF means
    /// the stream was truncated and data is missing.
    saw_index: bool,
    /// Whether the input is exhausted (index seen or clean EOF reached), so no
    /// more frames should be read even while in-flight blocks still drain.
    reading_done: bool,
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

    /// Returns the next decoded block in stream order, or `None` at a clean end.
    /// Serial: read one block and decompress it inline. Parallel: top up the
    /// in-flight window with freshly dispatched blocks, then wait for the oldest.
    /// Reaching EOF without having seen the index frame is [`BzstError::Truncated`].
    fn next_decoded(&mut self) -> BzstResult<Option<Vec<u8>>> {
        let Self { fr, strategy, saw_index, reading_done, .. } = self;
        match strategy {
            ReadStrategy::Serial(dec) => loop {
                match fr.next_frame()? {
                    None => return end_of_stream(*saw_index),
                    Some(Frame::Block { header, data }) => {
                        return Ok(Some(decode_block(dec, &header, data)?));
                    }
                    Some(Frame::Index(_)) => *saw_index = true,
                    Some(_) => {} // header/skippable: not payload, skip
                }
            },
            ReadStrategy::Parallel(pipeline) => {
                while !*reading_done && pipeline.inflight.len() < pipeline.max_inflight {
                    match fr.next_frame()? {
                        None => *reading_done = true,
                        Some(Frame::Block { header, data }) => {
                            pipeline.dispatch(header, data.to_vec());
                        }
                        // The index frame marks a complete stream and is the last
                        // frame; stop reading once it (or a clean EOF) is reached.
                        Some(Frame::Index(_)) => {
                            *saw_index = true;
                            *reading_done = true;
                        }
                        Some(_) => {} // header/skippable: not payload, skip
                    }
                }
                match pipeline.inflight.pop_front() {
                    Some(rx) => Ok(Some(recv_decoded(rx)?)),
                    None => end_of_stream(*saw_index),
                }
            }
        }
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
            match self.next_decoded()? {
                Some(block) => {
                    self.cur = block;
                    self.cur_pos = 0;
                }
                None => return Ok(0),
            }
        }
    }
}

/// Builder for [`BzstReader`].
pub struct BzstReaderBuilder<R> {
    inner: R,
    threads: Threads,
    max_block_bytes: u64,
}

impl<R: Read> BzstReaderBuilder<R> {
    fn new(inner: R) -> Self {
        Self { inner, threads: Threads::Serial, max_block_bytes: default_alloc_limit() }
    }

    /// Threading strategy for read-ahead decompression (default [`Threads::Serial`]).
    pub fn threads(mut self, threads: Threads) -> Self {
        self.threads = threads;
        self
    }

    /// Ceiling on a single block's compressed + uncompressed bytes before the
    /// reader rejects it as [`crate::BzstError::BlockTooLarge`] (default: ~95% of
    /// host RAM). Lower it to reject blocks larger than a caller will allocate.
    pub fn max_block_bytes(mut self, bytes: u64) -> Self {
        self.max_block_bytes = bytes;
        self
    }

    /// Builds the reader, validating and consuming the header frame.
    pub fn build(self) -> BzstResult<BzstReader<R>> {
        let BzstReaderBuilder { inner, threads, max_block_bytes } = self;
        let mut fr = FrameReader::new(inner, max_block_bytes);
        let header = match fr.next_frame()? {
            Some(Frame::Header(h)) => h,
            _ => return Err(BzstError::Malformed("stream does not start with a header frame")),
        };
        let strategy = match threads {
            Threads::Serial => ReadStrategy::Serial(ZstdDecompressor::new()?),
            Threads::Owned(n) => ReadStrategy::Parallel(ReadPipeline::new(Pool::new(n)?)),
            Threads::Shared(pool) => ReadStrategy::Parallel(ReadPipeline::new(pool)),
        };
        Ok(BzstReader {
            fr,
            header,
            strategy,
            cur: Vec::new(),
            cur_pos: 0,
            saw_index: false,
            reading_done: false,
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
    max_block_bytes: u64,
}

impl<R: Read + Seek> SeekableReader<R> {
    /// Opens a seekable reader, loading the header and index. If the index frame
    /// is present but corrupt, the index is rebuilt from the block-header frames
    /// so a damaged index never makes the file unreadable.
    pub fn new(mut inner: R) -> BzstResult<Self> {
        inner.seek(SeekFrom::Start(0))?;
        let header = Header::read_from(&mut inner)?;
        let index = match Index::read_from(&mut inner) {
            Ok(index) => index,
            Err(BzstError::CorruptIndex) => {
                inner.seek(SeekFrom::Start(0))?;
                Index::rebuild(&mut inner)?
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            inner,
            index,
            header,
            dec: ZstdDecompressor::new()?,
            cached_block: None,
            cache: Vec::new(),
            pos: 0,
            max_block_bytes: default_alloc_limit(),
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
        if entry.block_length > self.max_block_bytes {
            return Err(BzstError::BlockTooLarge {
                requested: entry.block_length,
                limit: self.max_block_bytes,
            });
        }
        self.inner.seek(SeekFrom::Start(entry.block_offset))?;
        let mut block = vec![0u8; entry.block_length as usize];
        self.inner.read_exact(&mut block)?;
        let bh = BlockHeader::parse_frame(&block)?;
        check_block_fits(bh.compressed_size, bh.uncompressed_size, self.max_block_bytes)?;
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
            SeekFrom::Start(o) => Some(o),
            SeekFrom::End(o) => total.checked_add_signed(o),
            SeekFrom::Current(o) => self.pos.checked_add_signed(o),
        };
        match new {
            Some(n) => {
                self.pos = n;
                Ok(n)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "seek to a negative position")),
        }
    }
}

// Internal per-reader decompression strategy.
enum ReadStrategy {
    Serial(ZstdDecompressor),
    Parallel(ReadPipeline),
}

/// The parallel decompression pipeline: a worker [`Pool`] plus a bounded window
/// of in-flight blocks, each awaiting its decoded bytes on a one-shot channel,
/// held in stream order.
struct ReadPipeline {
    pool: Pool,
    inflight: VecDeque<Receiver<BzstResult<Vec<u8>>>>,
    max_inflight: usize,
}

impl ReadPipeline {
    fn new(pool: Pool) -> Self {
        let max_inflight = max_blocks_for_threads(pool.0.current_num_threads());
        Self { pool, inflight: VecDeque::new(), max_inflight }
    }

    /// Hands one compressed block to a worker paired with a one-shot channel,
    /// appending the receiving end to the in-flight window. The worker decodes on
    /// a pool thread (reusing a thread-local zstd context) and sends the bytes
    /// back; the calling thread reads receivers back in stream order.
    fn dispatch(&mut self, header: BlockHeader, compressed: Vec<u8>) {
        let (tx, rx) = mpsc::channel();
        self.pool.0.spawn_fifo(move || {
            // If the receiver is gone (the reader aborted on an earlier error),
            // the send fails harmlessly and this block's work is discarded.
            let _ = tx.send(decode_block_owned(header, compressed));
        });
        self.inflight.push_back(rx);
    }
}

thread_local! {
    /// One reusable zstd decompression context per worker thread. Decompression
    /// takes no settings, so a single lazily-built context per thread suffices.
    static WORKER_DECOMPRESSOR: RefCell<Option<ZstdDecompressor>> = const { RefCell::new(None) };
}

/// Signals the end of the forward stream: a clean end once the index frame has
/// been seen, or [`BzstError::Truncated`] if EOF arrived without it (data lost).
fn end_of_stream(saw_index: bool) -> BzstResult<Option<Vec<u8>>> {
    if saw_index {
        Ok(None)
    } else {
        Err(BzstError::Truncated)
    }
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

/// Decompresses one owned block on a worker thread, reusing that thread's
/// lazily-built zstd context.
fn decode_block_owned(header: BlockHeader, compressed: Vec<u8>) -> BzstResult<Vec<u8>> {
    WORKER_DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(ZstdDecompressor::new()?);
        }
        decode_block(slot.as_mut().expect("decompressor was just set"), &header, &compressed)
    })
}

/// Receives one decoded block from its one-shot channel. A closed channel means
/// the worker vanished (panicked) before sending — surface that as a thread
/// error rather than hanging or unwrapping into a panic.
fn recv_decoded(rx: Receiver<BzstResult<Vec<u8>>>) -> BzstResult<Vec<u8>> {
    rx.recv().unwrap_or_else(|_| {
        Err(BzstError::Thread("a decompression worker stopped before returning a block".into()))
    })
}
