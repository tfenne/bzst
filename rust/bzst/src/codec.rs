//! The zstd codec seam.
//!
//! [`ZstdCompressor`]/[`ZstdDecompressor`] are thin, reusable-context wrappers
//! over the `zstd` crate that handle *only* zstd data frames — never bzst
//! framing. They are `pub(crate)`: the parallel pipeline needs a "compress one
//! block" primitive that runs on worker threads separate from the single-
//! threaded frame serialization, and this is that primitive. Reusing one
//! instance across many blocks amortizes the zstd context allocation.

use crate::BzstResult;

/// A reusable zstd compression context. One per thread (compression mutates the
/// context, hence `&mut self`).
pub(crate) struct ZstdCompressor {
    inner: zstd::bulk::Compressor<'static>,
}

impl ZstdCompressor {
    /// Creates a compressor at `level`, optionally writing a zstd content
    /// checksum into each produced data frame.
    pub(crate) fn new(level: i32, content_checksum: bool) -> BzstResult<Self> {
        let mut inner = zstd::bulk::Compressor::new(level)?;
        inner.include_checksum(content_checksum)?;
        // We store block sizes ourselves; still cheap and useful for `bzst inspect`.
        inner.include_contentsize(true)?;
        Ok(Self { inner })
    }

    /// Compresses `src` into `dst` (which must be at least [`Self::bound`] bytes),
    /// returning the number of bytes written.
    pub(crate) fn compress(&mut self, src: &[u8], dst: &mut [u8]) -> BzstResult<usize> {
        Ok(self.inner.compress_to_buffer(src, dst)?)
    }

    /// Worst-case compressed size for `src_len` input bytes (`ZSTD_compressBound`).
    pub(crate) fn bound(src_len: usize) -> usize {
        zstd::zstd_safe::compress_bound(src_len)
    }
}

/// A reusable zstd decompression context. One per thread.
pub(crate) struct ZstdDecompressor {
    inner: zstd::bulk::Decompressor<'static>,
}

impl ZstdDecompressor {
    pub(crate) fn new() -> BzstResult<Self> {
        Ok(Self { inner: zstd::bulk::Decompressor::new()? })
    }

    /// Decompresses one data frame `src` into `dst` (pre-sized from the block
    /// header's stored uncompressed size), returning bytes written.
    pub(crate) fn decompress(&mut self, src: &[u8], dst: &mut [u8]) -> BzstResult<usize> {
        Ok(self.inner.decompress_to_buffer(src, dst)?)
    }
}
