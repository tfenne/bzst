//! End-to-end tests of the `bzst` command-line interface: the gzip/bgzip-style
//! in-place file model, the mode flags, and text-mode record alignment. Each
//! test drives the real binary in a private temp directory.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bzst::Index;

const BZST: &str = env!("CARGO_BIN_EXE_bzst");

/// A scratch directory that removes itself on drop, so a mid-test assertion panic
/// can't leak it — a dep-free stand-in for `tempfile::TempDir`.
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fresh, empty scratch directory unique to `tag` (and this process).
fn scratch(tag: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("bzst_cli_{}_{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

/// Runs `bzst args...` with `dir` as the working directory.
fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(BZST).args(args).current_dir(dir).output().unwrap()
}

/// Compressible, line-oriented text.
fn text(lines: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..lines {
        writeln!(v, "line {i} the quick brown beast leaps over the lazy dog").unwrap();
    }
    v
}

#[test]
fn in_place_compress_removes_input_and_creates_archive() {
    let dir = scratch("compress_inplace");
    let data = text(20_000);
    fs::write(dir.join("a.txt"), &data).unwrap();

    let out = run(&dir, &["-l", "9", "a.txt"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!dir.join("a.txt").exists(), "input should be removed");
    let archive = fs::read(dir.join("a.txt.bzst")).unwrap();
    assert_eq!(bzst::decompress(&archive).unwrap(), data);
}

#[test]
fn decompress_in_place_restores_and_removes_archive() {
    let dir = scratch("decompress_inplace");
    let data = text(15_000);
    fs::write(dir.join("a.txt"), &data).unwrap();
    assert!(run(&dir, &["a.txt"]).status.success());

    let out = run(&dir, &["-d", "a.txt.bzst"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!dir.join("a.txt.bzst").exists(), "archive should be removed");
    assert_eq!(fs::read(dir.join("a.txt")).unwrap(), data);
}

#[test]
fn keep_flag_preserves_input() {
    let dir = scratch("keep");
    fs::write(dir.join("a.txt"), text(1000)).unwrap();

    assert!(run(&dir, &["-k", "a.txt"]).status.success());
    assert!(dir.join("a.txt").exists(), "-k keeps the input");
    assert!(dir.join("a.txt.bzst").exists());
}

#[test]
fn stdout_flag_writes_stdout_and_keeps_input() {
    let dir = scratch("stdout");
    let data = text(5000);
    fs::write(dir.join("a.txt"), &data).unwrap();

    let out = run(&dir, &["-c", "a.txt"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(bzst::decompress(&out.stdout).unwrap(), data);
    assert!(dir.join("a.txt").exists(), "-c keeps the input");
    assert!(!dir.join("a.txt.bzst").exists(), "-c writes no file");
}

#[test]
fn force_required_to_overwrite_existing_output() {
    let dir = scratch("force");
    fs::write(dir.join("a.txt"), text(1000)).unwrap();
    assert!(run(&dir, &["-k", "a.txt"]).status.success()); // creates a.txt.bzst

    let refused = run(&dir, &["-k", "a.txt"]);
    assert!(!refused.status.success(), "must refuse to overwrite without -f");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));

    assert!(run(&dir, &["-k", "-f", "a.txt"]).status.success(), "-f overwrites");
}

#[test]
fn test_mode_passes_clean_and_fails_corrupt() {
    let dir = scratch("test");
    fs::write(dir.join("a.txt"), text(20_000)).unwrap();
    assert!(run(&dir, &["-k", "a.txt"]).status.success());

    let good = run(&dir, &["-t", "a.txt.bzst"]);
    assert!(good.status.success(), "{}", String::from_utf8_lossy(&good.stderr));
    assert!(String::from_utf8_lossy(&good.stdout).contains("OK"));

    let mut bytes = fs::read(dir.join("a.txt.bzst")).unwrap();
    assert!(bytes.len() > 300);
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF; // flip a byte deep in the compressed data
    fs::write(dir.join("bad.bzst"), &bytes).unwrap();

    let bad = run(&dir, &["-t", "bad.bzst"]);
    assert!(!bad.status.success(), "a corrupted file must fail -t");
}

#[test]
fn list_reports_blocks_and_sizes() {
    let dir = scratch("list");
    fs::write(dir.join("a.txt"), text(50_000)).unwrap();
    assert!(run(&dir, &["-k", "-b", "65536", "a.txt"]).status.success());

    let out = run(&dir, &["--list", "a.txt.bzst"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("blocks"), "{s}");
    assert!(s.contains("block sizes"), "{s}");
    assert!(s.contains("ratio"), "{s}");
}

#[test]
fn cat_concatenates_verbatim_to_stdout() {
    let dir = scratch("cat");
    let a = text(3000);
    let b = text(2000);
    fs::write(dir.join("a.txt"), &a).unwrap();
    fs::write(dir.join("b.txt"), &b).unwrap();
    assert!(run(&dir, &["-k", "a.txt"]).status.success());
    assert!(run(&dir, &["-k", "b.txt"]).status.success());

    let out = run(&dir, &["--cat", "a.txt.bzst", "b.txt.bzst"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(bzst::decompress(&out.stdout).unwrap(), expected);
}

#[test]
fn text_mode_never_splits_a_record() {
    // Fixed-width 37-byte FASTQ records; 37 does not divide the 40000-byte target,
    // so a size-based cut would land mid-record and be caught below.
    const RECORD_LEN: usize = 37;
    let dir = scratch("text");
    let num_records = 20_000usize;
    let mut fastq = Vec::new();
    for i in 0..num_records {
        write!(fastq, "@read{i:07}\nACGTACGTAC\n+\nIIIIIIIIII\n").unwrap();
    }
    assert_eq!(fastq.len(), RECORD_LEN * num_records);
    fs::write(dir.join("reads.fq"), &fastq).unwrap();

    let out =
        run(&dir, &["-k", "-m", "text", "--lines-per-record", "4", "-b", "40000", "reads.fq"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let bytes = fs::read(dir.join("reads.fq.bzst")).unwrap();
    let index = Index::read_from(&mut Cursor::new(&bytes)).unwrap();
    assert!(index.len() > 1, "test needs multiple blocks");
    for entry in index.entries() {
        assert_eq!(
            entry.uncompressed_offset as usize % RECORD_LEN,
            0,
            "block begins mid-record at offset {}",
            entry.uncompressed_offset
        );
    }
    assert_eq!(bzst::decompress(&bytes).unwrap(), fastq);
}

#[test]
fn decompress_unknown_suffix_errors() {
    let dir = scratch("suffix");
    fs::write(dir.join("data.bin"), b"not a bzst file").unwrap();

    let out = run(&dir, &["-d", "data.bin"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown suffix"));
}

#[test]
fn output_flag_rejects_multiple_inputs() {
    let dir = scratch("multi_output");
    fs::write(dir.join("a.txt"), text(100)).unwrap();
    fs::write(dir.join("b.txt"), text(100)).unwrap();

    let out = run(&dir, &["-o", "out.bzst", "a.txt", "b.txt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("multiple input"));
}

#[test]
fn conflicting_mode_flags_are_rejected() {
    let dir = scratch("modes");
    fs::write(dir.join("a.txt"), text(100)).unwrap();

    let out = run(&dir, &["-d", "--list", "a.txt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("at most one"));
}

#[test]
fn auto_detects_text_and_keeps_records_whole() {
    const RECORD_LEN: usize = 37;
    let dir = scratch("auto_text");
    let mut fastq = Vec::new();
    for i in 0..20_000 {
        write!(fastq, "@read{i:07}\nACGTACGTAC\n+\nIIIIIIIIII\n").unwrap();
    }
    fs::write(dir.join("reads.fq"), &fastq).unwrap();

    // No -m: auto must detect text and, with 4-line records, keep them whole.
    let out = run(&dir, &["-k", "--lines-per-record", "4", "-b", "40000", "reads.fq"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let bytes = fs::read(dir.join("reads.fq.bzst")).unwrap();
    let index = Index::read_from(&mut Cursor::new(&bytes)).unwrap();
    assert!(index.len() > 1, "test needs multiple blocks");
    for entry in index.entries() {
        assert_eq!(
            entry.uncompressed_offset as usize % RECORD_LEN,
            0,
            "auto text mode split a record at offset {}",
            entry.uncompressed_offset
        );
    }
}

#[test]
fn auto_uses_binary_blocking_for_binary_input() {
    let dir = scratch("auto_bin");
    // Binary: contains NUL bytes and no newlines, so auto must pick binary.
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| match (i % 256) as u8 {
            b'\n' => 0, // ensure NUL bytes and remove all newlines
            b => b,
        })
        .collect();
    fs::write(dir.join("blob"), &data).unwrap();

    assert!(run(&dir, &["-k", "-b", "65536", "blob"]).status.success());

    let bytes = fs::read(dir.join("blob.bzst")).unwrap();
    let index = Index::read_from(&mut Cursor::new(&bytes)).unwrap();
    assert!(index.len() > 1, "test needs multiple blocks");
    // Binary blocking cuts every block but the last at exactly the block size;
    // text blocking would have made one huge block (no newlines to break on).
    for i in 0..index.len() - 1 {
        assert_eq!(
            index.uncompressed_block_size(i).unwrap(),
            65536,
            "auto should have chosen binary (fixed-size) blocking"
        );
    }
}

#[test]
fn mode_bin_forces_fixed_size_blocks_on_text() {
    let dir = scratch("bin_override");
    let mut fastq = Vec::new();
    for i in 0..20_000 {
        write!(fastq, "@read{i:07}\nACGTACGTAC\n+\nIIIIIIIIII\n").unwrap();
    }
    fs::write(dir.join("reads.fq"), &fastq).unwrap();

    // -m bin must ignore line boundaries even though the input is text.
    assert!(run(&dir, &["-k", "-m", "bin", "-b", "40000", "reads.fq"]).status.success());

    let bytes = fs::read(dir.join("reads.fq.bzst")).unwrap();
    let index = Index::read_from(&mut Cursor::new(&bytes)).unwrap();
    assert!(index.len() > 1);
    // A fixed 40000-byte cut splits a 37-byte record (37 does not divide 40000).
    assert_eq!(index.uncompressed_block_size(0).unwrap(), 40000);
}

#[test]
fn decompress_of_truncated_file_errors_and_keeps_input() {
    let dir = scratch("truncated");
    fs::write(dir.join("a.txt"), text(50_000)).unwrap();
    assert!(run(&dir, &["-b", "4096", "a.txt"]).status.success()); // -> a.txt.bzst

    // Lop off the second half of the archive (drops blocks and the index).
    let archive = fs::read(dir.join("a.txt.bzst")).unwrap();
    fs::write(dir.join("a.txt.bzst"), &archive[..archive.len() / 2]).unwrap();

    let out = run(&dir, &["-d", "a.txt.bzst"]);
    assert!(!out.status.success(), "truncated decompress must fail, not report success");
    assert!(dir.join("a.txt.bzst").exists(), "the input must not be deleted on failure");
}

#[test]
fn test_detects_index_corruption() {
    let dir = scratch("t_index");
    fs::write(dir.join("a.txt"), text(50_000)).unwrap();
    assert!(run(&dir, &["-k", "-b", "8192", "a.txt"]).status.success());

    // Corrupt a byte in the trailing index frame's Total field (blocks untouched).
    let mut bytes = fs::read(dir.join("a.txt.bzst")).unwrap();
    let n = bytes.len();
    let index_offset = u64::from_le_bytes(bytes[n - 12..n - 4].try_into().unwrap()) as usize;
    bytes[index_offset + 18] ^= 0xFF;
    fs::write(dir.join("bad.bzst"), &bytes).unwrap();

    assert!(!run(&dir, &["-t", "bad.bzst"]).status.success(), "index corruption must fail -t");
}

#[test]
fn output_equals_input_is_rejected() {
    let dir = scratch("same_file");
    let data = text(1000);
    fs::write(dir.join("a.txt"), &data).unwrap();

    let out = run(&dir, &["-f", "-o", "a.txt", "a.txt"]);
    assert!(!out.status.success(), "output == input must be rejected");
    assert_eq!(fs::read(dir.join("a.txt")).unwrap(), data, "the input must be untouched");
}

#[test]
#[cfg(unix)]
fn hard_link_of_input_is_rejected_as_output() {
    // A hard link to the input refers to the *same file*; writing to it via -o
    // would truncate the input. same_file must catch this by device+inode, since
    // the two names canonicalize to different paths.
    let dir = scratch("hardlink");
    let data = text(1000);
    fs::write(dir.join("a.txt"), &data).unwrap();
    fs::hard_link(dir.join("a.txt"), dir.join("b.txt")).unwrap();

    let out = run(&dir, &["-f", "-o", "b.txt", "a.txt"]);
    assert!(!out.status.success(), "a hard link of the input must be rejected as output");
    assert_eq!(fs::read(dir.join("a.txt")).unwrap(), data, "the input must be untouched");
}

#[test]
fn bin_mode_with_lines_per_record_is_rejected() {
    let dir = scratch("bin_lines");
    fs::write(dir.join("a.txt"), text(1000)).unwrap();

    let out = run(&dir, &["-m", "bin", "--lines-per-record", "4", "a.txt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no effect"));
}

#[test]
fn text_mode_handles_trailing_partial_record() {
    let dir = scratch("partial_record");
    // Three whole 4-line FASTQ records, then a final record missing two lines.
    let mut fastq = Vec::new();
    for i in 0..3 {
        write!(fastq, "@r{i}\nACGT\n+\nIIII\n").unwrap();
    }
    fastq.extend_from_slice(b"@r3\nACGT\n");
    fs::write(dir.join("reads.fq"), &fastq).unwrap();

    let out = run(&dir, &["-k", "-m", "text", "--lines-per-record", "4", "-b", "16", "reads.fq"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let bytes = fs::read(dir.join("reads.fq.bzst")).unwrap();
    assert_eq!(bzst::decompress(&bytes).unwrap(), fastq, "trailing partial record must round-trip");
}
