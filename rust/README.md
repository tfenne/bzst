# bzst (Rust)

A reference implementation of **bzst** ("beast") — block-compressed zstd — a parallel, seekable, `zstd`-compatible container format. This tracks the working-draft spec in [`../spec/bzst.typ`](../spec/bzst.typ).

Status: **early but working.** The core format (baseline profile) round-trips, is decodable by stock `zstd`, supports serial and parallel (de)compression, seekable random access, and derived-format skippable-frame injection/reading. See [Open questions & TODOs](#open-questions--todos).

## Workspace

- **`bzst/`** — the library crate. Depends on `zstd` (C libzstd), `rayon`, `crc32fast`, and `libc` (host-memory query used to bound per-block allocations against corrupt size fields).
- **`bzst-cli/`** — the `bzst` binary: a single, gzip/bgzip-style tool (compress by default; `-d`, `-t`, `--list`, `--cat`).

## Quickstart

```sh
cargo build --release
cargo test                     # 114 tests
cargo ci-fmt && cargo ci-clippy && cargo ci-test   # the CI gate set

# CLI — gzip/bgzip-style: compresses in place by default, removing the input.
target/release/bzst -@8 -l9 data.txt      # -> data.txt.bzst (removes data.txt)
target/release/bzst -d data.txt.bzst      # -> data.txt
target/release/bzst -c data.txt > d.bzst  # write to stdout, keep input
target/release/bzst -t data.txt.bzst      # test integrity
target/release/bzst --list data.txt.bzst  # header, block sizes, ratio
target/release/bzst --cat a.bzst b.bzst > all.bzst   # verbatim concatenate
zstd -d -c data.txt.bzst                  # stock zstd decodes a baseline file, too

# --mode auto (default) detects text vs binary; text mode ends blocks on line
# boundaries. --lines-per-record keeps N-line records (e.g. FASTQ) whole.
target/release/bzst --lines-per-record 4 reads.fastq   # auto-detected, record-safe
```

## Library API

```rust
use std::io::{Read, Write};
use bzst::{BzstWriter, BzstReader, SeekableReader, Threads};

// Write (serial or parallel; `end_block()` for record alignment;
// `write_skippable_frame()` for derived metadata).
let mut w = BzstWriter::builder(Vec::new())
    .level(3).block_size(1 << 20).threads(Threads::Owned(8))
    .build()?;
w.write_all(payload)?;
let bytes: Vec<u8> = w.finish()?;   // finish() is required — it writes the index

// Read forward (implements Read).
let mut out = Vec::new();
BzstReader::new(std::io::Cursor::new(&bytes))?.read_to_end(&mut out)?;

// Random access (implements Read + Seek over the uncompressed content).
let mut sr = SeekableReader::new(std::io::Cursor::new(&bytes))?;
let mut buf = vec![0u8; 4096];
sr.read_range(1_000_000, &mut buf)?;

// Index lookups that read only the file's tail and one index partition.
let mut lazy = bzst::LazyIndex::open(std::io::Cursor::new(&bytes))?;
let entry = lazy.entry_for_offset(1_000_000)?;

// One-shot helpers.
let c = bzst::compress(payload, 3)?;
let d = bzst::decompress(&c)?;
```

The zstd codec and raw frame I/O are internal (`pub(crate)`); the public surface is the high-level reader/writer, the value types (`Header`, `BlockHeader`, `Index`, `IndexEntry`, `OwnedFrame`), `LazyIndex` for on-demand index lookups, the `Frames` iterator (for derived formats reading their skippable metadata), and the free functions (`compress`, `decompress`, `concat`, `detect`, `header_of`).

## What's implemented

- Baseline profile: header / block-header / data / index frames, per the spec's provisional layout and magic numbers.
- Serial + pipelined-parallel writer and reader (own-threads or a shared `Pool`): blocks (de)compress on worker threads while the calling thread does ordered I/O, so compute overlaps I/O and the parallel output is byte-identical to serial.
- Partitioned, zstd-compressed index: per-block `u32` columns in partitions of 4,096 blocks by default (`index_partition_entries` / `--index-partition-entries`), with a fixed-width directory at the end of the file. `Index::read_from` decodes it all; `LazyIndex` reads only the file's tail and one partition per lookup. The index tail also records `Blocks_End`, so `blocks_end()` and `index_offset()` bracket any frames a derived format wrote after its last block. `Index::read_from` and `Index::rebuild` (forward pass) agree.
- Block sizes are `u32`, so a block is limited to just under 4 GiB; the writer reports anything larger as `ExceedsFormatLimit`.
- Seekable random access (`SeekableReader`) over the uncompressed stream.
- Structural-frame CRC32 checksums (zlib's `crc32()`); data-frame zstd content checksums (on by default).
- Robust decode: truncation is detected (a stream missing its trailing index errors instead of silently returning partial output), corrupt or oversized block sizes are rejected with a clean `BlockTooLarge` error rather than an OOM abort (capped at ~95% of host RAM), and a corrupt-but-present index is transparently rebuilt from the block-header frames.
- Derived-format skippable-frame injection (`write_skippable_frame`) and reading (`Frames`).
- Verbatim `cat` (`bzst::concat`): concatenates files by copying compressed blocks (and derived-format skippable frames) through with no recompression, regenerating a single combined header and index.
- `bzst` CLI: a gzip/bgzip-style single tool — compress by default, `-d`/`-t`/`--list`/`--cat`, in-place file handling (`-c`/`-o`/`-k`/`-f`), and `--mode auto|bin|text` (auto detects text vs binary, on files or pipes) with `--lines-per-record` for record-safe blocking.

## Open questions & TODOs

- **Provisional wire constants** (structural magic `0x184D2A5B`, EOF magic `0x8F92EA5B`) — see the spec's open issue on magic-number selection.
- `SeekableReader` loads the whole index on open; it could use `LazyIndex` instead to suit high-latency storage.
- **Dictionary profile** is out of v1 (see the spec's open issue on it); the codec seam and `Profiles` bit are reserved for it.
- No standalone `bzst::verify` library function yet; the CLI's `-t` composes a streaming decode (validating structural + content checksums) with an index-vs-rebuild check.
- The `Stored` block flag is never set by the writer (advisory; zstd framing is authoritative).
