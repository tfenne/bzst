//! Round-trip correctness: serial and parallel writers/readers, block sizes,
//! levels, and the high-level helpers.

mod common;

use std::io::{Read, Write};

use bzst::{BzstReader, BzstWriter, Pool, Threads};
use common::{pseudo_text, random_bytes};

fn roundtrip(
    data: &[u8],
    block_size: usize,
    level: i32,
    write: fn() -> Threads,
    read: fn() -> Threads,
) {
    let mut w = BzstWriter::builder(Vec::new())
        .level(level)
        .block_size(block_size)
        .threads(write())
        .build()
        .unwrap();
    w.write_all(data).unwrap();
    let bytes = w.finish().unwrap();

    let mut out = Vec::new();
    BzstReader::builder(std::io::Cursor::new(&bytes))
        .threads(read())
        .build()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, data, "round-trip mismatch (block_size={block_size}, level={level})");
}

#[test]
fn roundtrip_empty_input() {
    roundtrip(&[], 1 << 16, 3, || Threads::Serial, || Threads::Serial);
}

#[test]
fn roundtrip_tiny_input() {
    roundtrip(b"hello, beast!", 1 << 16, 3, || Threads::Serial, || Threads::Serial);
}

#[test]
fn roundtrip_random_multiblock_serial() {
    let data = random_bytes(500_000, 42);
    roundtrip(&data, 64 << 10, 3, || Threads::Serial, || Threads::Serial);
}

#[test]
fn roundtrip_text_high_level_helpers() {
    let data = pseudo_text(300_000, 1);
    let compressed = bzst::compress(&data, 9).unwrap();
    assert!(compressed.len() < data.len(), "text should compress");
    assert_eq!(bzst::decompress(&compressed).unwrap(), data);
}

#[test]
fn roundtrip_parallel_write_serial_read() {
    let data = pseudo_text(2_000_000, 7);
    roundtrip(&data, 64 << 10, 5, || Threads::Owned(4), || Threads::Serial);
}

#[test]
fn roundtrip_parallel_read_serial_write() {
    let data = random_bytes(2_000_000, 11);
    roundtrip(&data, 96 << 10, 3, || Threads::Serial, || Threads::Owned(4));
}

#[test]
fn roundtrip_parallel_both_ends() {
    let data = pseudo_text(3_000_000, 99);
    roundtrip(&data, 128 << 10, 3, || Threads::Owned(4), || Threads::Owned(4));
}

#[test]
fn roundtrip_shared_pool() {
    let pool = Pool::new(3).unwrap();
    let data = pseudo_text(1_500_000, 5);
    let mut w = BzstWriter::builder(Vec::new())
        .block_size(96 << 10)
        .threads(Threads::Shared(pool.clone()))
        .build()
        .unwrap();
    w.write_all(&data).unwrap();
    let bytes = w.finish().unwrap();
    let mut out = Vec::new();
    BzstReader::builder(std::io::Cursor::new(&bytes))
        .threads(Threads::Shared(pool))
        .build()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, data);
}

#[test]
fn serial_and_parallel_produce_identical_bytes() {
    // Same block size + level + content-checksum => byte-identical output, since
    // each block is compressed deterministically; parallelism only reorders work.
    let data = pseudo_text(1_000_000, 3);
    let mut serial = BzstWriter::builder(Vec::new()).block_size(64 << 10).build().unwrap();
    serial.write_all(&data).unwrap();
    let serial = serial.finish().unwrap();

    let mut parallel = BzstWriter::builder(Vec::new())
        .block_size(64 << 10)
        .threads(Threads::Owned(4))
        .build()
        .unwrap();
    parallel.write_all(&data).unwrap();
    let parallel = parallel.finish().unwrap();

    assert_eq!(serial, parallel);
}

#[test]
fn end_block_cuts_record_aligned_blocks() {
    let mut w = BzstWriter::builder(Vec::new()).block_size(1 << 20).build().unwrap();
    for rec in ["record-one", "record-two", "record-three"] {
        w.write_all(rec.as_bytes()).unwrap();
        w.end_block().unwrap();
    }
    let bytes = w.finish().unwrap();

    let index = bzst::Index::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(index.len(), 3, "one block per end_block despite huge block_size");
    assert_eq!(bzst::decompress(&bytes).unwrap(), b"record-onerecord-tworecord-three");
}

#[test]
fn many_writes_reassemble_correctly() {
    // Exercise the Write impl across many small writes crossing block boundaries.
    let mut w = BzstWriter::builder(Vec::new()).block_size(4096).build().unwrap();
    let mut expected = Vec::new();
    for i in 0..5000u32 {
        let chunk = format!("line {i}: {}\n", "x".repeat((i % 40) as usize));
        w.write_all(chunk.as_bytes()).unwrap();
        expected.extend_from_slice(chunk.as_bytes());
    }
    let bytes = w.finish().unwrap();
    assert_eq!(bzst::decompress(&bytes).unwrap(), expected);
}
