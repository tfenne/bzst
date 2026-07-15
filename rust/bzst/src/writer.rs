//! Writing bzst streams: [`BzstWriter`] plus its threading model.
//!
//! The writer accumulates uncompressed bytes into blocks, compresses them
//! (serially, or in parallel via a worker [`Pool`]), writes `[block-header]
//! [data]` in order, and appends the index on [`BzstWriter::finish`].
//!
//! The parallel path is a **pipeline, not a batch barrier**: each block is
//! handed to the pool paired with a one-shot channel, and the receiving ends are
//! kept in submission order. Workers compress later blocks while the calling
//! thread writes earlier ones, so compression and I/O overlap. The calling
//! thread reads the finished blocks back in order and writes them itself, so `W`
//! never leaves that thread (no `Send` bound) and the bytes are identical to the
//! serial path. A bounded in-flight window supplies back-pressure, capping how
//! much memory the outstanding blocks can hold.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver};

use crate::codec::ZstdCompressor;
use crate::frame::{EncodedBlock, FrameWriter, Header};
use crate::index::IndexBuilder;
use crate::threads::{max_blocks_for_threads, Pool, Threads};
use crate::{
    BzstError, BzstResult, Profiles, DEFAULT_BLOCK_SIZE, DEFAULT_LEVEL, SKIPPABLE_MAGIC_MAX,
    SKIPPABLE_MAGIC_MIN, STRUCTURAL_MAGIC,
};

/// Writes a bzst stream to an underlying `Write`. Implements [`std::io::Write`]
/// (auto-cutting blocks at the target size) plus explicit block and skippable-
/// frame control.
///
/// **You must call [`BzstWriter::finish`]** — dropping the writer without it
/// leaves the file without its index frame.
pub struct BzstWriter<W: Write> {
    fw: FrameWriter<W>,
    index: IndexBuilder,
    staging: Vec<u8>,
    block_size: usize,
    strategy: Strategy,
}

impl<W: Write> BzstWriter<W> {
    /// Creates a writer with default settings, writing the header immediately.
    pub fn new(inner: W) -> BzstResult<Self> {
        Self::builder(inner).build()
    }

    /// Starts a [`BzstWriterBuilder`] to configure level, block size, threading, etc.
    pub fn builder(inner: W) -> BzstWriterBuilder<W> {
        BzstWriterBuilder::new(inner)
    }

    /// Forces a block boundary now: emits whatever is staged as one block. Use
    /// this to keep record-based data record-aligned (no record straddles a block).
    pub fn end_block(&mut self) -> BzstResult<()> {
        if !self.staging.is_empty() {
            let block = std::mem::take(&mut self.staging);
            self.emit_block(block)?;
        }
        Ok(())
    }

    /// Injects a derived-format skippable frame at the current position (after
    /// flushing any staged block). `magic` must be in the zstd skippable range
    /// and must not be bzst's own structural magic.
    pub fn write_skippable_frame(
        &mut self,
        magic: u32,
        payload: impl AsRef<[u8]>,
    ) -> BzstResult<()> {
        if !(SKIPPABLE_MAGIC_MIN..=SKIPPABLE_MAGIC_MAX).contains(&magic)
            || magic == STRUCTURAL_MAGIC
        {
            return Err(BzstError::BadSkippableMagic(magic));
        }
        self.end_block()?;
        self.drain_inflight()?;
        self.fw.write_skippable(magic, payload.as_ref())
    }

    /// Flushes the last block, writes the index + EOF trailer, and returns the
    /// underlying writer.
    pub fn finish(mut self) -> BzstResult<W> {
        self.end_block()?;
        self.drain_inflight()?;
        let index = std::mem::replace(&mut self.index, IndexBuilder::new()).finish();
        self.fw.write_index(&index)?;
        self.fw.flush()?;
        Ok(self.fw.into_inner())
    }

    /// Compresses and writes one block. Serial: compress inline and write it now.
    /// Parallel: dispatch it to the pool and, once the in-flight window is full,
    /// write the oldest finished block — bounding memory and preserving order.
    fn emit_block(&mut self, block: Vec<u8>) -> BzstResult<()> {
        let Self { fw, index, strategy, .. } = self;
        match strategy {
            Strategy::Serial(zc) => {
                let encoded = EncodedBlock::encode(zc, &block)?;
                Self::write_and_index(fw, index, &encoded)?;
            }
            Strategy::Parallel(pipeline) => {
                pipeline.dispatch(block);
                if pipeline.inflight.len() > pipeline.max_inflight {
                    let rx = pipeline.inflight.pop_front().expect("window is non-empty");
                    let encoded = recv_encoded(rx)?;
                    Self::write_and_index(fw, index, &encoded)?;
                }
            }
        }
        Ok(())
    }

    /// Writes every in-flight parallel block, in submission order, then returns.
    /// A no-op on the serial path (nothing is ever in flight).
    fn drain_inflight(&mut self) -> BzstResult<()> {
        let Self { fw, index, strategy, .. } = self;
        if let Strategy::Parallel(pipeline) = strategy {
            while let Some(rx) = pipeline.inflight.pop_front() {
                let encoded = recv_encoded(rx)?;
                Self::write_and_index(fw, index, &encoded)?;
            }
        }
        Ok(())
    }

    /// Writes one encoded block's frames and records it in the index. Takes the
    /// fields it needs (not `&mut self`) so callers can hold a disjoint borrow of
    /// `strategy` at the same time.
    fn write_and_index(
        fw: &mut FrameWriter<W>,
        index: &mut IndexBuilder,
        encoded: &EncodedBlock,
    ) -> BzstResult<()> {
        let offset = fw.write_encoded_block(encoded)?;
        index.push(offset, encoded.on_disk_len(), encoded.header.uncompressed_size);
        Ok(())
    }
}

impl<W: Write> Write for BzstWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.staging.extend_from_slice(buf);
        while self.staging.len() >= self.block_size {
            let block: Vec<u8> = self.staging.drain(..self.block_size).collect();
            self.emit_block(block)?;
        }
        Ok(buf.len())
    }

    /// Flushes the underlying writer. Does **not** force a block boundary (staged
    /// data waits for the target size or [`BzstWriter::finish`]); use
    /// [`BzstWriter::end_block`] to cut a block.
    fn flush(&mut self) -> io::Result<()> {
        self.fw.flush()?;
        Ok(())
    }
}

/// Builder for [`BzstWriter`].
pub struct BzstWriterBuilder<W> {
    inner: W,
    level: i32,
    block_size: usize,
    content_checksum: bool,
    format_signature: [u8; 4],
    threads: Threads,
}

impl<W: Write> BzstWriterBuilder<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            level: DEFAULT_LEVEL,
            block_size: DEFAULT_BLOCK_SIZE,
            content_checksum: true,
            format_signature: [0; 4],
            threads: Threads::Serial,
        }
    }

    /// zstd compression level (default [`crate::DEFAULT_LEVEL`]).
    pub fn level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }

    /// Target uncompressed block size (default [`crate::DEFAULT_BLOCK_SIZE`]).
    pub fn block_size(mut self, bytes: usize) -> Self {
        self.block_size = bytes.max(1);
        self
    }

    /// Whether to write a zstd content checksum into each data frame (default `true`).
    pub fn content_checksum(mut self, on: bool) -> Self {
        self.content_checksum = on;
        self
    }

    /// A 4-byte opaque derived-format tag written into the header (default none).
    pub fn format_signature(mut self, sig: [u8; 4]) -> Self {
        self.format_signature = sig;
        self
    }

    /// Threading strategy (default [`Threads::Serial`]).
    pub fn threads(mut self, threads: Threads) -> Self {
        self.threads = threads;
        self
    }

    /// Builds the writer, validating the level and writing the header frame.
    pub fn build(self) -> BzstResult<BzstWriter<W>> {
        let BzstWriterBuilder {
            inner,
            level,
            block_size,
            content_checksum,
            format_signature,
            threads,
        } = self;
        if !zstd::compression_level_range().contains(&level) {
            return Err(BzstError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("zstd level {level} out of range"),
            )));
        }
        let strategy = match threads {
            Threads::Serial => Strategy::Serial(ZstdCompressor::new(level, content_checksum)?),
            Threads::Owned(n) => {
                Strategy::Parallel(Pipeline::new(Pool::new(n)?, level, content_checksum))
            }
            Threads::Shared(pool) => {
                Strategy::Parallel(Pipeline::new(pool, level, content_checksum))
            }
        };
        let mut fw = FrameWriter::new(inner);
        fw.write_header(&Header::new(format_signature, Profiles::BASELINE))?;
        Ok(BzstWriter { fw, index: IndexBuilder::new(), staging: Vec::new(), block_size, strategy })
    }
}

// Internal per-writer compression strategy.
enum Strategy {
    Serial(ZstdCompressor),
    Parallel(Pipeline),
}

/// The parallel compression pipeline: a worker [`Pool`] plus a bounded window of
/// in-flight blocks, each awaiting its compressed result on a one-shot channel,
/// held in submission (== output) order.
struct Pipeline {
    pool: Pool,
    inflight: VecDeque<Receiver<BzstResult<EncodedBlock>>>,
    max_inflight: usize,
    level: i32,
    content_checksum: bool,
}

impl Pipeline {
    fn new(pool: Pool, level: i32, content_checksum: bool) -> Self {
        let max_inflight = max_blocks_for_threads(pool.0.current_num_threads());
        Self { pool, inflight: VecDeque::new(), max_inflight, level, content_checksum }
    }

    /// Hands `block` to a worker paired with a one-shot channel, appending the
    /// receiving end to the in-flight window. The worker compresses on a pool
    /// thread (reusing a thread-local zstd context) and sends the result back;
    /// the calling thread later reads receivers back in order. `spawn_fifo` keeps
    /// workers biased toward submission order, so the oldest block — the one the
    /// writer blocks on next — tends to finish first.
    fn dispatch(&mut self, block: Vec<u8>) {
        let (tx, rx) = mpsc::channel();
        let level = self.level;
        let content_checksum = self.content_checksum;
        self.pool.0.spawn_fifo(move || {
            // If the receiver is gone (the writer aborted on an earlier error),
            // the send fails harmlessly and this block's work is discarded.
            let _ = tx.send(compress_block(level, content_checksum, &block));
        });
        self.inflight.push_back(rx);
    }
}

thread_local! {
    /// One reusable zstd context per worker thread, tagged with the settings it
    /// was built for. A pool shared across writers of different levels rebuilds
    /// only when the settings actually change, not on every block.
    static WORKER_COMPRESSOR: RefCell<Option<(i32, bool, ZstdCompressor)>> =
        const { RefCell::new(None) };
}

/// Compresses one block on a worker thread, reusing (or lazily building) that
/// thread's zstd context for the requested settings.
fn compress_block(level: i32, content_checksum: bool, block: &[u8]) -> BzstResult<EncodedBlock> {
    WORKER_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let reusable = matches!(&*slot, Some((l, c, _)) if *l == level && *c == content_checksum);
        if !reusable {
            *slot = Some((level, content_checksum, ZstdCompressor::new(level, content_checksum)?));
        }
        let zc = &mut slot.as_mut().expect("compressor was just set").2;
        EncodedBlock::encode(zc, block)
    })
}

/// Receives one compressed block from its one-shot channel. A closed channel
/// means the worker vanished (panicked) before sending — surface that as a
/// thread error rather than hanging or unwrapping into a panic.
fn recv_encoded(rx: Receiver<BzstResult<EncodedBlock>>) -> BzstResult<EncodedBlock> {
    rx.recv().unwrap_or_else(|_| {
        Err(BzstError::Thread("a compression worker stopped before returning a block".into()))
    })
}
