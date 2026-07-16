//! Seekable random access, the index, and index reconstruction.

mod common;

use std::io::{Read, Seek, SeekFrom, Write};

use bzst::{BzstWriter, Index, SeekableReader};
use common::{pseudo_text, random_bytes};

fn write_file(data: &[u8], block_size: usize) -> Vec<u8> {
    let mut w = BzstWriter::builder(Vec::new()).block_size(block_size).build().unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

#[test]
fn read_range_in_the_middle() {
    let data = pseudo_text(1_000_000, 2);
    let bytes = write_file(&data, 64 << 10);
    let mut sr = SeekableReader::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(sr.total_uncompressed(), data.len() as u64);

    let off = 500_123usize;
    let mut buf = vec![0u8; 4096];
    let n = sr.read_range(off as u64, &mut buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(buf, data[off..off + buf.len()]);
}

#[test]
fn read_range_spans_block_boundaries() {
    let data = random_bytes(300_000, 77);
    let bytes = write_file(&data, 64 << 10);
    let mut sr = SeekableReader::new(std::io::Cursor::new(&bytes)).unwrap();

    // A 10 KiB range starting near a 64 KiB block boundary spans two blocks.
    let off = 60_000usize;
    let mut buf = vec![0u8; 10_000];
    let n = sr.read_range(off as u64, &mut buf).unwrap();
    assert_eq!(n, buf.len());
    assert_eq!(buf, data[off..off + buf.len()]);
}

#[test]
fn read_range_at_and_past_end() {
    let data = pseudo_text(100_000, 4);
    let bytes = write_file(&data, 32 << 10);
    let mut sr = SeekableReader::new(std::io::Cursor::new(&bytes)).unwrap();

    let mut buf = vec![0u8; 16];
    assert_eq!(sr.read_range(data.len() as u64, &mut buf).unwrap(), 0);
    assert_eq!(sr.read_range(data.len() as u64 + 1000, &mut buf).unwrap(), 0);
}

#[test]
fn seek_then_read_exact() {
    let data = pseudo_text(800_000, 8);
    let bytes = write_file(&data, 48 << 10);
    let mut sr = SeekableReader::new(std::io::Cursor::new(&bytes)).unwrap();

    let off = 321_000u64;
    sr.seek(SeekFrom::Start(off)).unwrap();
    let mut buf = vec![0u8; 5000];
    sr.read_exact(&mut buf).unwrap();
    assert_eq!(buf, data[off as usize..off as usize + buf.len()]);

    // SeekFrom::End
    sr.seek(SeekFrom::End(-100)).unwrap();
    let mut tail = Vec::new();
    sr.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, &data[data.len() - 100..]);
}

#[test]
fn written_index_matches_forward_rebuild() {
    let data = pseudo_text(500_000, 12);
    let bytes = write_file(&data, 64 << 10);

    let read = Index::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();
    let rebuilt = Index::rebuild(std::io::Cursor::new(&bytes)).unwrap();

    assert!(read.len() > 1);
    assert_eq!(read.total_uncompressed(), data.len() as u64);
    assert_eq!(read.total_uncompressed(), rebuilt.total_uncompressed());
    assert_eq!(read.entries(), rebuilt.entries());
    // First block starts right after the header frame (28 bytes).
    assert_eq!(read.entry(0).unwrap().block_offset, 28);
    assert_eq!(read.entry(0).unwrap().uncompressed_offset, 0);
}

#[test]
fn block_for_offset_is_correct() {
    let data = random_bytes(400_000, 5);
    let bytes = write_file(&data, 64 << 10);
    let index = Index::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();

    for &off in &[0u64, 1, 64 * 1024, 200_000, 399_999] {
        let bi = index.block_for_offset(off).unwrap();
        let e = index.entry(bi).unwrap();
        let next = index
            .entry(bi + 1)
            .map(|n| n.uncompressed_offset)
            .unwrap_or(index.total_uncompressed());
        assert!(e.uncompressed_offset <= off && off < next);
    }
    assert_eq!(index.block_for_offset(data.len() as u64), None);
}

#[test]
fn seek_before_start_is_an_error() {
    let data = pseudo_text(100_000, 9);
    let bytes = write_file(&data, 32 << 10);
    let mut sr = SeekableReader::new(std::io::Cursor::new(&bytes)).unwrap();

    // std::io::Seek requires seeking before byte 0 to error, not clamp to 0.
    assert!(sr.seek(SeekFrom::Current(-1)).is_err());
    assert!(sr.seek(SeekFrom::End(-(data.len() as i64) - 1)).is_err());
    // A valid seek still works.
    assert_eq!(sr.seek(SeekFrom::Start(10)).unwrap(), 10);
}

#[test]
fn uncompressed_block_size_partitions_the_stream() {
    let data = pseudo_text(1_000_000, 15);
    let bytes = write_file(&data, 64 << 10);
    let index = Index::read_from(&mut std::io::Cursor::new(&bytes)).unwrap();

    let n = index.len();
    assert!(n > 1);
    let sizes: Vec<u64> = (0..n).map(|i| index.uncompressed_block_size(i).unwrap()).collect();

    // The per-block sizes partition the uncompressed stream exactly.
    assert_eq!(sizes.iter().sum::<u64>(), data.len() as u64);
    // Raw splitting cuts every block but the last at exactly the block size.
    for &size in &sizes[..n - 1] {
        assert_eq!(size, 64 << 10);
    }
    assert!(*sizes.last().unwrap() <= 64 << 10);
    // An out-of-range block index yields None.
    assert_eq!(index.uncompressed_block_size(n), None);
}
