//! Interop with stock `zstd`, format detection, header signatures, and
//! skippable-frame injection/reading.

mod common;

use std::io::Write;
use std::process::Command;

use bzst::{BzstWriter, OwnedFrame};
use common::{pseudo_text, tmp_path};

#[test]
fn stock_zstd_decodes_a_baseline_file() {
    let data = pseudo_text(400_000, 21);
    let bytes = bzst::compress(&data, 5).unwrap();

    let path = tmp_path("interop_baseline.bzst");
    std::fs::write(&path, &bytes).unwrap();

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
    assert_eq!(output.stdout, data, "stock zstd must reproduce the original bytes");
}

#[test]
fn stock_zstd_decodes_a_file_with_skippable_metadata() {
    let mut w = BzstWriter::builder(Vec::new()).block_size(64 << 10).build().unwrap();
    w.write_all(&pseudo_text(200_000, 1)).unwrap();
    w.write_skippable_frame(0x184D_2A50, b"derived-metadata-goes-here").unwrap();
    w.write_all(&pseudo_text(200_000, 2)).unwrap();
    let bytes = w.finish().unwrap();

    let mut expected = pseudo_text(200_000, 1);
    expected.extend_from_slice(&pseudo_text(200_000, 2));

    let path = tmp_path("interop_skippable.bzst");
    std::fs::write(&path, &bytes).unwrap();
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
    assert_eq!(output.stdout, expected, "zstd must skip the metadata frame");
}

#[test]
fn detect_recognizes_bzst_and_rejects_others() {
    let bzst_bytes = bzst::compress(b"hello", 3).unwrap();
    assert!(bzst::detect(&bzst_bytes));

    assert!(!bzst::detect(b"not a bzst file"));
    assert!(!bzst::detect(&[]));

    // A plain zstd frame must NOT be mistaken for bzst.
    let plain = zstd::bulk::compress(b"hello world hello world", 3).unwrap();
    assert!(!bzst::detect(&plain));
}

#[test]
fn header_signature_round_trips() {
    let mut w = BzstWriter::builder(Vec::new()).format_signature(*b"BAM2").build().unwrap();
    w.write_all(b"x").unwrap();
    let bytes = w.finish().unwrap();

    let header = bzst::header_of(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(header.format_signature, *b"BAM2");
    assert_eq!(header.format_version, bzst::FORMAT_VERSION);
    assert!(header.profiles.is_baseline());

    // The 8-byte type magic is [BZST][tag] at offsets 10 and 14.
    assert_eq!(&bytes[10..14], b"BZST");
    assert_eq!(&bytes[14..18], b"BAM2");
}

#[test]
fn skippable_frames_are_read_back_by_derived_formats() {
    let mut w = BzstWriter::builder(Vec::new()).block_size(1 << 20).build().unwrap();
    w.write_all(b"payload-part-1 ").unwrap();
    w.write_skippable_frame(0x184D_2A50, b"meta-a").unwrap();
    w.write_all(b"payload-part-2").unwrap();
    w.write_skippable_frame(0x184D_2A5C, b"meta-b").unwrap();
    let bytes = w.finish().unwrap();

    // The payload decodes without the metadata.
    assert_eq!(bzst::decompress(&bytes).unwrap(), b"payload-part-1 payload-part-2");

    // A derived format can walk the frames and pull its metadata back.
    let skippables: Vec<(u32, Vec<u8>)> = bzst::Frames::new(std::io::Cursor::new(&bytes))
        .map(Result::unwrap)
        .filter_map(|f| match f {
            OwnedFrame::Skippable { magic, payload } => Some((magic, payload)),
            _ => None,
        })
        .collect();
    assert_eq!(
        skippables,
        vec![(0x184D_2A50, b"meta-a".to_vec()), (0x184D_2A5C, b"meta-b".to_vec())]
    );
}

#[test]
fn rejects_reserved_skippable_magic() {
    let mut w = BzstWriter::builder(Vec::new()).build().unwrap();
    // bzst's own structural magic is off-limits to derived formats.
    let err = w.write_skippable_frame(bzst::STRUCTURAL_MAGIC, b"nope");
    assert!(err.is_err());
    // Out-of-range magics are rejected too.
    assert!(w.write_skippable_frame(0xDEAD_BEEF, b"nope").is_err());
}
