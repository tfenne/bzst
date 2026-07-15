//! Verbatim concatenation: block-copy fidelity, index rebuild, stock-zstd
//! interop, and the frames dropped along the way.

mod common;

use std::io::{Cursor, Write};
use std::process::Command;

use bzst::{BzstWriter, Index, OwnedFrame, SeekableReader};
use common::{pseudo_text, tmp_path};

fn compress_file(data: &[u8], block_size: usize, level: i32) -> Vec<u8> {
    let mut w =
        BzstWriter::builder(Vec::new()).block_size(block_size).level(level).build().unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

/// The on-disk bytes of every `[block-header][data]` region, in order, sliced
/// straight from the file via the index. Used to prove blocks are copied
/// byte-for-byte.
fn block_regions(bytes: &[u8]) -> Vec<Vec<u8>> {
    let index = Index::read_from(&mut Cursor::new(bytes)).unwrap();
    index
        .entries()
        .iter()
        .map(|e| {
            bytes[e.block_offset as usize..(e.block_offset + e.block_length) as usize].to_vec()
        })
        .collect()
}

/// The `(magic, payload)` of every skippable frame in the file, in order.
fn skippable_frames(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
    bzst::Frames::new(Cursor::new(bytes))
        .map(Result::unwrap)
        .filter_map(|f| match f {
            OwnedFrame::Skippable { magic, payload } => Some((magic, payload)),
            _ => None,
        })
        .collect()
}

/// The kind of each frame in the file, in order.
fn frame_kinds(bytes: &[u8]) -> Vec<&'static str> {
    bzst::Frames::new(Cursor::new(bytes))
        .map(Result::unwrap)
        .map(|f| match f {
            OwnedFrame::Header(_) => "header",
            OwnedFrame::Block(_) => "block",
            OwnedFrame::Skippable { .. } => "skippable",
            OwnedFrame::Index(_) => "index",
        })
        .collect()
}

#[test]
fn concat_decompresses_to_the_concatenation_of_inputs() {
    let a = pseudo_text(300_000, 1);
    let b = pseudo_text(250_000, 2);
    let fa = compress_file(&a, 64 << 10, 5);
    let fb = compress_file(&b, 64 << 10, 5);

    let out = bzst::concat(vec![Cursor::new(&fa), Cursor::new(&fb)], Vec::new()).unwrap();

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(bzst::decompress(&out).unwrap(), expected);
}

#[test]
fn concat_copies_blocks_byte_for_byte() {
    // Compress at a high level so that any accidental re-compression at the
    // default level would change the bytes and fail the comparison below.
    let a = pseudo_text(400_000, 7);
    let b = pseudo_text(180_000, 8);
    let fa = compress_file(&a, 32 << 10, 19);
    let fb = compress_file(&b, 32 << 10, 19);

    let out = bzst::concat(vec![Cursor::new(&fa), Cursor::new(&fb)], Vec::new()).unwrap();

    let mut expected = block_regions(&fa);
    expected.extend(block_regions(&fb));
    assert_eq!(block_regions(&out), expected, "blocks must be copied verbatim, not recompressed");
}

#[test]
fn concat_rebuilds_a_searchable_index() {
    let a = pseudo_text(500_000, 3);
    let b = pseudo_text(500_000, 4);
    let fa = compress_file(&a, 64 << 10, 5);
    let fb = compress_file(&b, 64 << 10, 5);

    let out = bzst::concat(vec![Cursor::new(&fa), Cursor::new(&fb)], Vec::new()).unwrap();

    // The trailer-located index agrees with a forward rebuild over the blocks.
    let read = Index::read_from(&mut Cursor::new(&out)).unwrap();
    let rebuilt = Index::rebuild(Cursor::new(&out)).unwrap();
    assert_eq!(read.entries(), rebuilt.entries());
    assert_eq!(read.total_uncompressed(), (a.len() + b.len()) as u64);

    // Block count is the sum of the inputs' block counts.
    let na = Index::read_from(&mut Cursor::new(&fa)).unwrap().len();
    let nb = Index::read_from(&mut Cursor::new(&fb)).unwrap().len();
    assert_eq!(read.len(), na + nb);

    // Random access straddling the A|B boundary returns the right bytes.
    let mut sr = SeekableReader::new(Cursor::new(&out)).unwrap();
    let off = a.len() - 100;
    let mut buf = vec![0u8; 200];
    assert_eq!(sr.read_range(off as u64, &mut buf).unwrap(), 200);
    let mut expected = a[off..].to_vec();
    expected.extend_from_slice(&b[..100]);
    assert_eq!(buf, expected);
}

#[test]
fn concat_result_is_decodable_by_stock_zstd() {
    let a = pseudo_text(200_000, 5);
    let b = pseudo_text(200_000, 6);
    let fa = compress_file(&a, 64 << 10, 5);
    let fb = compress_file(&b, 64 << 10, 5);

    let out = bzst::concat(vec![Cursor::new(&fa), Cursor::new(&fb)], Vec::new()).unwrap();

    let path = tmp_path("concat_interop.bzst");
    std::fs::write(&path, &out).unwrap();
    let output = match Command::new("zstd").args(["-d", "-c", path.to_str().unwrap()]).output() {
        Ok(o) => o,
        Err(_) => {
            eprintln!("skipping: `zstd` CLI not found on PATH");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let _ = std::fs::remove_file(&path);
    assert!(output.status.success(), "zstd -d failed: {}", String::from_utf8_lossy(&output.stderr));

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(output.stdout, expected, "stock zstd must reproduce both inputs' bytes");
}

#[test]
fn concat_preserves_derived_format_frames_in_place() {
    let mut w = BzstWriter::builder(Vec::new()).block_size(64 << 10).build().unwrap();
    w.write_all(&pseudo_text(100_000, 1)).unwrap();
    w.write_skippable_frame(0x184D_2A50, b"derived-metadata").unwrap();
    w.write_all(&pseudo_text(100_000, 2)).unwrap();
    let input = w.finish().unwrap();

    let out = bzst::concat(vec![Cursor::new(&input)], Vec::new()).unwrap();

    // The derived skippable frame survives, magic and payload intact.
    assert_eq!(skippable_frames(&out), vec![(0x184D_2A50u32, b"derived-metadata".to_vec())]);

    // ...and stays where it was: between data blocks, not hoisted to an end.
    let kinds = frame_kinds(&out);
    assert_eq!(kinds.first(), Some(&"header"));
    assert_eq!(kinds.last(), Some(&"index"));
    let at = kinds.iter().position(|k| *k == "skippable").unwrap();
    assert!(
        kinds[..at].contains(&"block") && kinds[at + 1..].contains(&"block"),
        "the derived frame stays sandwiched between data blocks: {kinds:?}"
    );

    // The payload itself is unchanged.
    let mut expected = pseudo_text(100_000, 1);
    expected.extend_from_slice(&pseudo_text(100_000, 2));
    assert_eq!(bzst::decompress(&out).unwrap(), expected);
}

#[test]
fn concat_preserves_derived_frames_from_every_input() {
    let build = |seed: u64, meta: &[u8]| {
        let mut w = BzstWriter::builder(Vec::new()).build().unwrap();
        w.write_all(&pseudo_text(50_000, seed)).unwrap();
        w.write_skippable_frame(0x184D_2A5C, meta).unwrap();
        w.finish().unwrap()
    };
    let fa = build(1, b"meta-a");
    let fb = build(2, b"meta-b");

    let out = bzst::concat(vec![Cursor::new(&fa), Cursor::new(&fb)], Vec::new()).unwrap();

    // Both inputs' derived frames appear, in input order.
    assert_eq!(
        skippable_frames(&out),
        vec![(0x184D_2A5Cu32, b"meta-a".to_vec()), (0x184D_2A5Cu32, b"meta-b".to_vec())]
    );
}

#[test]
fn concat_uses_the_first_input_header_signature() {
    let mut wa = BzstWriter::builder(Vec::new()).format_signature(*b"BAM2").build().unwrap();
    wa.write_all(b"alpha").unwrap();
    let fa = wa.finish().unwrap();
    let fb = compress_file(b"beta", 1 << 20, 3); // default signature [0; 4]

    let out = bzst::concat(vec![Cursor::new(&fa), Cursor::new(&fb)], Vec::new()).unwrap();

    let header = bzst::header_of(Cursor::new(&out)).unwrap();
    assert_eq!(header.format_signature, *b"BAM2");
    assert_eq!(bzst::decompress(&out).unwrap(), b"alphabeta");
}

#[test]
fn concat_of_no_inputs_is_a_valid_empty_file() {
    let out = bzst::concat(Vec::<Cursor<&[u8]>>::new(), Vec::new()).unwrap();

    assert!(bzst::detect(&out));
    assert_eq!(bzst::decompress(&out).unwrap(), b"");
    let index = Index::read_from(&mut Cursor::new(&out)).unwrap();
    assert_eq!(index.len(), 0);
    assert_eq!(index.total_uncompressed(), 0);
}

#[test]
fn concat_of_empty_bzst_files_has_no_blocks() {
    let ea = compress_file(b"", 1 << 20, 3);
    let eb = compress_file(b"", 1 << 20, 3);

    let out = bzst::concat(vec![Cursor::new(&ea), Cursor::new(&eb)], Vec::new()).unwrap();

    assert_eq!(bzst::decompress(&out).unwrap(), b"");
    assert_eq!(Index::read_from(&mut Cursor::new(&out)).unwrap().len(), 0);
}

#[test]
fn concat_rejects_a_non_bzst_input() {
    let good = compress_file(b"ok", 1 << 20, 3);
    let garbage = b"this is definitely not a bzst file".to_vec();

    let result = bzst::concat(vec![Cursor::new(&good[..]), Cursor::new(&garbage[..])], Vec::new());
    assert!(result.is_err(), "a non-bzst input must be rejected");
}
