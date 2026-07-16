//! Shared threading primitives for parallel block (de)compression, used
//! symmetrically by the writer (compression) and the reader (read-ahead
//! decompression). Kept in their own module so neither peer module depends on
//! the other for this cross-cutting concern.

use std::sync::Arc;

use crate::{BzstError, BzstResult};

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

/// Blocks to batch per parallel round for `threads` workers: enough to keep every
/// worker fed with a little slack, with a floor for small pools. Shared by the
/// writer's flush target and the reader's read-ahead batch so the two stay in step.
pub(crate) fn max_blocks_for_threads(threads: usize) -> usize {
    (threads * 4).max(8)
}
