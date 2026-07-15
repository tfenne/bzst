//! Robustness against truncated, corrupt, and crafted input: readers must return
//! a clean error (never panic, abort, or silently drop data), and a damaged index
//! must not make an otherwise-intact file unreadable.

mod common;

use std::io::{Cursor, Read, Write};

use bzst::{BzstReader, BzstWriter, Index, SeekableReader};
use common::pseudo_text;

fn compress_blocks(data: &[u8], block_size: usize) -> Vec<u8> {
    let mut w = BzstWriter::builder(Vec::new()).block_size(block_size).build().unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

/// Absolute offset of the index frame, read from the 12-byte EOF trailer.
fn index_offset(bytes: &[u8]) -> usize {
    let n = bytes.len();
    u64::from_le_bytes(bytes[n - 12..n - 4].try_into().unwrap()) as usize
}

#[test]
fn truncation_at_a_block_boundary_is_detected_not_silently_partial() {
    let data = pseudo_text(500_000, 3);
    let bytes = compress_blocks(&data, 32 << 10);
    let index = Index::read_from(&mut Cursor::new(&bytes)).unwrap();
    assert!(index.len() > 2);
    let e0 = index.entry(0).unwrap();
    // A clean cut right after block 0 (drops the remaining blocks and the index).
    let truncated = bytes[..(e0.block_offset + e0.block_length) as usize].to_vec();

    // Streaming decode must error rather than report success with partial output.
    let mut r = BzstReader::new(Cursor::new(&truncated)).unwrap();
    assert!(r.read_to_end(&mut Vec::new()).is_err(), "truncated stream must error");
    assert!(bzst::decompress(&truncated).is_err());
}

#[test]
fn oversized_block_is_rejected_not_allocated() {
    let data = pseudo_text(200_000, 4);
    let bytes = compress_blocks(&data, 64 << 10);
    // Cap far below any real block: decoding must fail cleanly (BlockTooLarge),
    // never attempt the multi-KiB allocation the block header declares.
    let mut r = BzstReader::builder(Cursor::new(&bytes)).max_block_bytes(1024).build().unwrap();
    let err = r.read_to_end(&mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("safe limit"), "unexpected error: {err}");
}

#[test]
fn corrupt_index_still_decodes_and_seeks_via_rebuild() {
    let data = pseudo_text(300_000, 5);
    let bytes = compress_blocks(&data, 32 << 10);
    let mut corrupt = bytes.clone();
    // Flip a byte in the index frame's checksummed Total field; the data blocks
    // themselves are untouched.
    corrupt[index_offset(&bytes) + 18] ^= 0xFF;

    // The stored index no longer validates on its own,
    assert!(Index::read_from(&mut Cursor::new(&corrupt)).is_err());
    // but streaming decode still returns the full payload (spec: a damaged index
    // never breaks the file),
    assert_eq!(bzst::decompress(&corrupt).unwrap(), data);
    // and the seekable reader recovers by rebuilding the index from the blocks.
    let mut sr = SeekableReader::new(Cursor::new(&corrupt)).unwrap();
    assert_eq!(sr.total_uncompressed(), data.len() as u64);
    let mut buf = vec![0u8; 100];
    assert_eq!(sr.read_range(1000, &mut buf).unwrap(), 100);
    assert_eq!(buf, data[1000..1100]);
}
