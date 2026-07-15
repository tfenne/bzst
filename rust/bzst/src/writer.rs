//! Writing bzst streams: [`BzstWriter`] plus its threading model.
//!
//! The writer accumulates uncompressed bytes into blocks, compresses them
//! (serially, or in batched parallel via a [`Pool`]), writes `[block-header]
//! [data]` in order, and appends the index on [`BzstWriter::finish`]. Workers
//! only compress; the calling thread owns `W` and writes in order, so `W` needs
//! no `Send` bound.

use std::io::{self, Write};
use std::sync::Arc;

use rayon::prelude::*;

use crate::codec::ZstdCompressor;
use crate::frame::{EncodedBlock, FrameWriter, Header};
use crate::index::IndexBuilder;
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
    level: i32,
    content_checksum: bool,
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
        self.flush_batch()?;
        self.fw.write_skippable(magic, payload.as_ref())
    }

    /// Flushes the last block, writes the index + EOF trailer, and returns the
    /// underlying writer.
    pub fn finish(mut self) -> BzstResult<W> {
        self.end_block()?;
        self.flush_batch()?;
        let index = std::mem::replace(&mut self.index, IndexBuilder::new()).finish();
        self.fw.write_index(&index)?;
        self.fw.flush()?;
        Ok(self.fw.into_inner())
    }

    fn emit_block(&mut self, block: Vec<u8>) -> BzstResult<()> {
        let mut encoded = None;
        let mut need_flush = false;
        match &mut self.strategy {
            Strategy::Serial(zc) => encoded = Some(EncodedBlock::encode(zc, &block)?),
            Strategy::Parallel { batch, target, .. } => {
                batch.push(block);
                need_flush = batch.len() >= *target;
            }
        }
        if let Some(eb) = encoded {
            let offset = self.fw.write_encoded_block(&eb)?;
            self.index.push(offset, eb.on_disk_len(), eb.header.uncompressed_size);
        }
        if need_flush {
            self.flush_batch()?;
        }
        Ok(())
    }

    fn flush_batch(&mut self) -> BzstResult<()> {
        let (pool, blocks) = match &mut self.strategy {
            Strategy::Serial(_) => return Ok(()),
            Strategy::Parallel { pool, batch, .. } if batch.is_empty() => {
                let _ = pool;
                return Ok(());
            }
            Strategy::Parallel { pool, batch, .. } => (pool.clone(), std::mem::take(batch)),
        };
        let level = self.level;
        let checksum = self.content_checksum;
        let encoded: BzstResult<Vec<EncodedBlock>> = pool.0.install(|| {
            blocks
                .par_iter()
                .map_init(
                    || ZstdCompressor::new(level, checksum).expect("validated compressor config"),
                    |zc, block| EncodedBlock::encode(zc, block),
                )
                .collect()
        });
        for eb in encoded? {
            let offset = self.fw.write_encoded_block(&eb)?;
            self.index.push(offset, eb.on_disk_len(), eb.header.uncompressed_size);
        }
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
    max_inflight_blocks: usize,
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
            max_inflight_blocks: 0,
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

    /// Maximum blocks compressed-but-not-yet-written in parallel mode (backpressure).
    pub fn max_inflight_blocks(mut self, n: usize) -> Self {
        self.max_inflight_blocks = n;
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
            max_inflight_blocks,
        } = self;
        if !zstd::compression_level_range().contains(&level) {
            return Err(BzstError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("zstd level {level} out of range"),
            )));
        }
        let target = |threads: usize| {
            if max_inflight_blocks > 0 {
                max_inflight_blocks
            } else {
                (threads * 4).max(8)
            }
        };
        let strategy = match threads {
            Threads::Serial => Strategy::Serial(ZstdCompressor::new(level, content_checksum)?),
            Threads::Owned(n) => {
                let pool = Pool::new(n)?;
                let t = pool.0.current_num_threads();
                Strategy::Parallel { pool, batch: Vec::new(), target: target(t) }
            }
            Threads::Shared(pool) => {
                let t = pool.0.current_num_threads();
                Strategy::Parallel { pool, batch: Vec::new(), target: target(t) }
            }
        };
        let mut fw = FrameWriter::new(inner);
        fw.write_header(&Header::new(format_signature, Profiles::BASELINE))?;
        Ok(BzstWriter {
            fw,
            index: IndexBuilder::new(),
            staging: Vec::new(),
            block_size,
            level,
            content_checksum,
            strategy,
        })
    }
}

/// How a reader or writer parallelizes block (de)compression.
pub enum Threads {
    /// Compress/decompress inline on the calling thread.
    Serial,
    /// Own a fresh pool of `n` threads (`0` = all available cores).
    Owned(usize),
    /// Submit work to a shared [`Pool`].
    Shared(Pool),
}

/// A shared pool of worker threads, usable across many readers and writers.
#[derive(Clone)]
pub struct Pool(pub(crate) Arc<rayon::ThreadPool>);

impl Pool {
    /// Creates a pool with `threads` workers (`0` = all available cores).
    pub fn new(threads: usize) -> BzstResult<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|e| BzstError::Thread(e.to_string()))?;
        Ok(Pool(Arc::new(pool)))
    }
}

// Internal per-writer compression strategy.
enum Strategy {
    Serial(ZstdCompressor),
    Parallel { pool: Pool, batch: Vec<Vec<u8>>, target: usize },
}
