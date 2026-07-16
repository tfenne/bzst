//! Shared helpers for the integration tests. Test data is generated
//! programmatically so it is visible in the tests, never committed.
//!
//! Each test binary uses a subset of these, so allow the unused ones per-binary.
#![allow(dead_code)]

/// Deterministic pseudo-random (near-incompressible) bytes via a splitmix64-ish LCG.
pub fn random_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

/// Compressible, text-like data: a stream of words chosen by a deterministic LCG.
pub fn pseudo_text(n: usize, seed: u64) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "the ", "quick ", "brown ", "beast ", "leaps ", "over ", "lorem ", "ipsum ", "dolor ",
        "sit ", "amet ", "alpha ", "beta ", "gamma ", "\n",
    ];
    let mut s = seed;
    let mut out = Vec::with_capacity(n + 16);
    while out.len() < n {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.extend_from_slice(WORDS[(s >> 40) as usize % WORDS.len()].as_bytes());
    }
    out.truncate(n);
    out
}

/// A unique temp path for a test artifact (cleaned up by the caller). Tests run
/// concurrently within one process, so a per-process atomic counter keeps paths
/// distinct even if two callers pass the same `name`.
pub fn tmp_path(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bzst_test_{}_{}_{}", std::process::id(), unique, name))
}
